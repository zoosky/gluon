use std::fmt;

use base::ast;
use base::ast::{DisplayEnv, Type, RcKind, MutVisitor};
use base::scoped_map::ScopedMap;
use base::symbol::{Symbol, SymbolModule};
use base::types::{Typed, TcType, TcIdent, KindEnv, TypeEnv};
use base::error::Errors;

pub type Error = Errors<ast::Spanned<RenameError>>;

#[derive(Clone, Debug, PartialEq)]
pub enum RenameError {
    NoMatchingType {
        symbol: String,
        expected: ast::ASTType<String>,
        possible_types: Vec<ast::ASTType<String>>,
    },
}

impl fmt::Display for RenameError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            RenameError::NoMatchingType { ref symbol, ref expected, ref possible_types } => {
                try!(writeln!(f,
                              "Could not resolve a binding for `{}` with type `{}`",
                              symbol,
                              expected));
                try!(writeln!(f, "Possibilities:"));
                for typ in possible_types {
                    try!(writeln!(f, "{}", typ));
                }
                Ok(())
            }
        }
    }
}

struct Environment<'b> {
    env: &'b TypeEnv,
    stack: ScopedMap<Symbol, (Symbol, TcType)>,
    stack_types: ScopedMap<Symbol, TcType>,
}

impl<'a> KindEnv for Environment<'a> {
    fn find_kind(&self, _type_name: Symbol) -> Option<RcKind> {
        None
    }
}

impl<'a> TypeEnv for Environment<'a> {
    fn find_type(&self, id: &Symbol) -> Option<&TcType> {
        self.stack.get(id).map(|t| &t.1).or_else(|| self.env.find_type(id))
    }
    fn find_type_info(&self, id: &Symbol) -> Option<(&[ast::Generic<Symbol>], Option<&TcType>)> {
        self.stack_types
            .get(id)
            .map(|typ| (&[][..], Some(typ)))
            .or_else(|| self.env.find_type_info(id))
    }
    fn find_record(&self, _fields: &[Symbol]) -> Option<(&TcType, &TcType)> {
        None
    }
}

pub fn rename(symbols: &mut SymbolModule,
              env: &TypeEnv,
              expr: &mut ast::LExpr<TcIdent>)
              -> Result<(), Error> {
    struct RenameVisitor<'a: 'b, 'b> {
        symbols: &'b mut SymbolModule<'a>,
        env: Environment<'b>,
        errors: Error,
    }
    impl<'a, 'b> RenameVisitor<'a, 'b> {
        fn find_fields<'s>(&'s self, typ: &'s TcType) -> Option<&'s [ast::Field<Symbol, TcType>]> {
            // Walk through all type aliases
            match **self.remove_aliases(typ) {
                Type::Record { ref fields, .. } => Some(fields),
                _ => None,
            }
        }

        fn remove_aliases<'s>(&'s self, typ: &'s TcType) -> &'s TcType {
            let mut typ = typ;
            loop {
                match **typ {
                    ast::Type::Data(ast::TypeConstructor::Data(id), _) => {
                        match self.env.find_type_info(&id) {
                            Some((_, Some(real_type))) => {
                                typ = real_type;
                            }
                            _ => break,
                        }
                    }
                    _ => break,
                }
            }
            typ
        }

        fn new_pattern(&mut self, typ: &TcType, pattern: &mut ast::LPattern<TcIdent>) {
            match pattern.value {
                ast::Pattern::Record { ref mut fields, ref types, .. } => {
                    let field_types = self.find_fields(typ).expect("field_types").to_owned();
                    for field in fields.iter_mut() {
                        let field_type = field_types.iter()
                                                    .find(|field_type| field_type.name == field.0)
                                                    .expect("ICE: Existing field")
                                                    .typ
                                                    .clone();
                        let id = field.1.unwrap_or(field.0);
                        field.1 = Some(self.stack_var(id, pattern.location, field_type));
                    }
                    let record_type = self.remove_aliases(typ).clone();
                    let imported_types = match *record_type {
                        Type::Record { ref types, .. } => types,
                        _ => panic!(),
                    };
                    for &(name, _) in types {
                        let field_type = imported_types.iter()
                                                       .find(|field| field.name == name)
                                                       .expect("field_type");
                        self.stack_type(name, field_type.typ.name, field_type.typ.typ.clone());
                    }
                }
                ast::Pattern::Identifier(ref mut id) => {
                    let new_name = self.stack_var(id.name, pattern.location, id.typ.clone());
                    debug!("Rename pattern `{}` = `{}`",
                           self.symbols.string(&id.name),
                           self.symbols.string(&new_name));
                    id.name = new_name;
                }
                ast::Pattern::Constructor(ref mut id, ref mut args) => {
                    let typ = self.env
                                  .find_type(id.id())
                                  .expect("ICE: Expected constructor")
                                  .clone();
                    for (arg_type, arg) in ast::arg_iter(&typ).zip(args) {
                        arg.name = self.stack_var(arg.name, pattern.location, arg_type.clone());
                    }
                }
            }
        }

        fn stack_var(&mut self, id: Symbol, location: ast::Location, typ: TcType) -> Symbol {
            let old_id = id.clone();
            let name = self.symbols.string(&id).to_owned();
            let new_id = self.symbols.symbol(format!("{}:{}", name, location));
            self.env.stack.insert(old_id, (new_id, typ));
            new_id

        }

        fn stack_type(&mut self, id: Symbol, scoped_id: Symbol, real_type: TcType) {
            // Insert variant constructors into the local scope
            if let Type::Variants(ref variants) = *real_type {
                for &(name, ref typ) in variants {
                    self.env.stack.insert(name, (name, typ.clone()));
                }
            }
            // FIXME: Workaround so that both the types name in this module and its global
            // name are imported. Without this aliases may not be traversed properly
            self.env
                .stack_types
                .insert(scoped_id, real_type.clone());
            self.env.stack_types.insert(id, real_type);
        }

        fn rename(&self, id: Symbol, expected: &TcType) -> Option<Symbol> {
            self.env
                .stack
                .get_all(&id)
                .and_then(|bindings| {
                    if bindings.len() == 1 {
                        Some(bindings[0].0)
                    } else {
                        bindings.iter()
                                .rev()
                                .find(|bind| equivalent(&self.env, &bind.1, expected))
                                .map(|bind| bind.0)
                    }
                })
                .or_else(|| {
                    self.env.find_type(&id).and_then(|typ| {
                        if equivalent(&self.env, typ, &expected) {
                            Some(id)
                        } else {
                            None
                        }
                    })
                })
        }

        fn rename_expr(&mut self, expr: &mut ast::LExpr<TcIdent>) -> Result<(), RenameError> {
            match expr.value {
                ast::Expr::Identifier(ref mut id) => {
                    let new_id = self.rename(*id.id(), &id.typ);
                    if new_id.is_none() {
                        return Err(RenameError::NoMatchingType {
                            symbol: String::from(self.symbols.string(&id.name)),
                            expected: id.typ.clone_strings(&self.symbols),
                            possible_types: self.env
                                                .stack
                                                .get_all(id.id())
                                                .iter()
                                                .flat_map(|binds| {
                                                    binds.iter().map(|bind| {
                                                        bind.1.clone_strings(&self.symbols)
                                                    })
                                                })
                                                .collect(),
                        });
                    }
                    id.name = new_id.unwrap_or(id.name);
                }
                ast::Expr::Record { ref mut typ, ref mut exprs, .. } => {
                    let field_types = self.find_fields(&typ.typ).expect("field_types").to_owned();
                    for (field, &mut (id, ref mut maybe_expr)) in field_types.iter().zip(exprs) {
                        match *maybe_expr {
                            Some(ref mut expr) => self.visit_expr(expr),
                            None => {
                                let new_id = self.env
                                                 .stack
                                                 .get_all(&id)
                                                 .and_then(|bindings| {
                                                     if bindings.len() == 1 {
                                                         Some(bindings[0].0)
                                                     } else {
                                                         bindings.iter()
                                                                 .rev()
                                                                 .find(|bind| {
                                                                     equivalent(&self.env,
                                                                                &bind.1,
                                                                                &field.typ)
                                                                 })
                                                                 .map(|bind| bind.0)
                                                     }
                                                 });
                                *maybe_expr =
                                    Some(ast::no_loc(ast::Expr::Identifier(ast::TcIdent {
                                        name: new_id.unwrap_or(id),
                                        typ: field.typ.clone(),
                                    })));
                            }
                        }
                    }
                }
                ast::Expr::BinOp(ref mut l, ref mut id, ref mut r) => {
                    let new_id = self.env
                                     .stack
                                     .get_all(id.id())
                                     .and_then(|bindings| {
                                         if bindings.len() == 1 {
                                             Some(bindings[0].0)
                                         } else {
                                             bindings.iter()
                                                     .rev()
                                                     .find(|bind| bind.1 == id.typ)
                                                     .map(|bind| bind.0)
                                         }
                                     });
                    debug!("Rename {} = {}",
                           self.symbols.string(&id.name),
                           self.symbols.string(&new_id.unwrap_or(id.name)));
                    id.name = new_id.unwrap_or(id.name);
                    self.visit_expr(l);
                    self.visit_expr(r);
                }
                ast::Expr::Match(ref mut expr, ref mut alts) => {
                    self.visit_expr(expr);
                    for alt in alts {
                        self.env.stack.enter_scope();
                        let typ = expr.env_type_of(&self.env);
                        self.new_pattern(&typ, &mut alt.pattern);
                        self.visit_expr(&mut alt.expression);
                        self.env.stack.exit_scope();
                    }
                }
                ast::Expr::Let(ref mut bindings, ref mut expr) => {
                    self.env.stack.enter_scope();
                    let is_recursive = bindings.iter().all(|bind| bind.arguments.len() > 0);
                    for bind in bindings.iter_mut() {
                        if !is_recursive {
                            self.visit_expr(&mut bind.expression);
                        }
                        let typ = bind.env_type_of(&self.env);
                        self.new_pattern(&typ, &mut bind.name);
                    }
                    if is_recursive {
                        for bind in bindings {
                            self.env.stack.enter_scope();
                            for (typ, arg) in ast::arg_iter(&bind.type_of())
                                                  .zip(&mut bind.arguments) {
                                arg.name = self.stack_var(arg.name, expr.location, typ.clone());
                            }
                            self.visit_expr(&mut bind.expression);
                            self.env.stack.exit_scope();
                        }
                    }
                    self.visit_expr(expr);
                    self.env.stack.exit_scope();
                }
                ast::Expr::Lambda(ref mut lambda) => {
                    self.env.stack.enter_scope();
                    for (typ, arg) in ast::arg_iter(&lambda.id.typ).zip(&mut lambda.arguments) {
                        arg.name = self.stack_var(arg.name, expr.location, typ.clone());
                    }
                    self.visit_expr(&mut lambda.body);
                    self.env.stack.exit_scope();
                }
                ast::Expr::Type(ref bindings, ref mut expr) => {
                    self.env.stack_types.enter_scope();
                    for &ast::TypeBinding { ref name, ref typ } in bindings {
                        match **name {
                            Type::Data(ast::TypeConstructor::Data(id), _) => {
                                self.stack_type(id, id, typ.clone());
                            }
                            _ => panic!(),
                        }
                    }
                    self.visit_expr(expr);
                    self.env.stack_types.exit_scope();
                }
                _ => ast::walk_mut_expr(self, expr),
            }
            Ok(())
        }
    }
    impl<'a, 'b> MutVisitor for RenameVisitor<'a, 'b> {
        type T = ast::TcIdent<Symbol>;

        fn visit_expr(&mut self, expr: &mut ast::LExpr<Self::T>) {
            if let Err(err) = self.rename_expr(expr) {
                self.errors.error(ast::Spanned {
                    span: expr.span(&ast::TcIdentEnvWrapper(&self.symbols)),
                    value: err,
                });
            }
        }
    }
    let mut visitor = RenameVisitor {
        symbols: symbols,
        errors: Errors::new(),
        env: Environment {
            env: env,
            stack: ScopedMap::new(),
            stack_types: ScopedMap::new(),
        },
    };
    visitor.visit_expr(expr);
    if visitor.errors.has_errors() {
        Err(visitor.errors)
    } else {
        Ok(())
    }
}


use std::collections::HashMap;
use unify_type::TypeError;
use substitution::Substitution;
use instantiate::{Instantiator, AliasInstantiator};
use unify::{Error as UnifyError, Unifier, Unifiable, UnifierState};

pub fn equivalent(env: &TypeEnv, actual: &TcType, inferred: &TcType) -> bool {
    let inst = Instantiator::new();
    let subs = Substitution::new();
    let ref mut state = AliasInstantiator::new(&inst, env);
    let mut map = HashMap::new();
    let mut equiv = true;
    {
        let mut unifier = UnifierState {
            state: state,
            subs: &subs,
            unifier: Equivalent {
                map: &mut map,
                equiv: &mut equiv,
            },
        };
        unifier.try_match(actual, inferred);
    }
    equiv
}

struct Equivalent<'m> {
    map: &'m mut HashMap<Symbol, TcType>,
    equiv: &'m mut bool,
}

impl<'a, 'm> Unifier<AliasInstantiator<'a>, TcType> for Equivalent<'m> {
    fn report_error(_unifier: &mut UnifierState<AliasInstantiator<'a>, TcType, Self>,
                    _error: UnifyError<TcType, TypeError<Symbol>>) {
    }

    fn try_match(unifier: &mut UnifierState<AliasInstantiator<'a>, TcType, Self>,
                 l: &TcType,
                 r: &TcType)
                 -> Option<TcType> {
        let subs = unifier.subs;
        let l = subs.real(l);
        let r = subs.real(r);
        match (&**l, &**r) {
            (&Type::Generic(ref gl), &Type::Generic(ref gr)) if gl == gr => None,
            (&Type::Generic(ref gl), _) => {
                match unifier.unifier.map.get(&gl.id).cloned() {
                    Some(ref typ) => unifier.try_match(typ, r),
                    None => {
                        unifier.unifier.map.insert(gl.id, r.clone());
                        None
                    }
                }
            }
            _ => {
                let result = {
                    let next_unifier = UnifierState {
                        state: unifier.state,
                        subs: subs,
                        unifier: Equivalent {
                            map: unifier.unifier.map,
                            equiv: unifier.unifier.equiv,
                        },
                    };
                    l.zip_match(r, next_unifier)
                };
                match result {
                    Ok(typ) => typ,
                    Err(_) => {
                        *unifier.unifier.equiv = false;
                        None
                    }
                }
            }
        }
    }
}
