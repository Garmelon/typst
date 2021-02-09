//! Evaluation of syntax trees.

#[macro_use]
mod value;
mod capture;
mod ops;
mod scope;

pub use capture::*;
pub use scope::*;
pub use value::*;

use std::collections::HashMap;
use std::rc::Rc;

use super::*;
use crate::color::Color;
use crate::diag::{Diag, Feedback};
use crate::geom::{Angle, Length, Relative};
use crate::syntax::visit::Visit;
use crate::syntax::*;

/// Evaluate all expressions in a syntax tree.
///
/// The `scope` consists of the base definitions that are present from the
/// beginning (typically, the standard library).
pub fn eval(env: &mut Env, tree: &Tree, scope: &Scope) -> Pass<ExprMap> {
    let mut ctx = EvalContext::new(env, scope);
    let map = tree.eval(&mut ctx);
    Pass::new(map, ctx.feedback)
}

/// A map from expression to values to evaluated to.
///
/// The raw pointers point into the expressions contained in `tree`. Since
/// the lifetime is erased, `tree` could go out of scope while the hash map
/// still lives. While this could lead to lookup panics, it is not unsafe
/// since the pointers are never dereferenced.
pub type ExprMap = HashMap<*const Expr, Value>;

/// The context for evaluation.
#[derive(Debug)]
pub struct EvalContext<'a> {
    /// The environment from which resources are gathered.
    pub env: &'a mut Env,
    /// The active scopes.
    pub scopes: Scopes<'a>,
    /// The accumulated feedback.
    feedback: Feedback,
}

impl<'a> EvalContext<'a> {
    /// Create a new execution context with a base scope.
    pub fn new(env: &'a mut Env, scope: &'a Scope) -> Self {
        Self {
            env,
            scopes: Scopes::with_base(scope),
            feedback: Feedback::new(),
        }
    }

    /// Add a diagnostic to the feedback.
    pub fn diag(&mut self, diag: Spanned<Diag>) {
        self.feedback.diags.push(diag);
    }
}

/// Evaluate an expression.
pub trait Eval {
    /// The output of evaluating the expression.
    type Output;

    /// Evaluate the expression to the output value.
    fn eval(&self, ctx: &mut EvalContext) -> Self::Output;
}

impl Eval for Tree {
    type Output = ExprMap;

    fn eval(&self, ctx: &mut EvalContext) -> Self::Output {
        struct ExprVisitor<'a, 'b> {
            map: ExprMap,
            ctx: &'a mut EvalContext<'b>,
        }

        impl<'ast> Visit<'ast> for ExprVisitor<'_, '_> {
            fn visit_expr(&mut self, item: &'ast Expr) {
                self.map.insert(item as *const _, item.eval(self.ctx));
            }
        }

        let mut visitor = ExprVisitor { map: HashMap::new(), ctx };
        visitor.visit_tree(self);
        visitor.map
    }
}

impl Eval for Expr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> Self::Output {
        match self {
            Self::Lit(lit) => lit.eval(ctx),
            Self::Ident(v) => match ctx.scopes.get(&v) {
                Some(slot) => slot.borrow().clone(),
                None => {
                    ctx.diag(error!(v.span, "unknown variable"));
                    Value::Error
                }
            },
            Self::Array(v) => Value::Array(v.eval(ctx)),
            Self::Dict(v) => Value::Dict(v.eval(ctx)),
            Self::Template(v) => Value::Template(vec![v.eval(ctx)]),
            Self::Group(v) => v.eval(ctx),
            Self::Block(v) => v.eval(ctx),
            Self::Call(v) => v.eval(ctx),
            Self::Unary(v) => v.eval(ctx),
            Self::Binary(v) => v.eval(ctx),
            Self::Let(v) => v.eval(ctx),
            Self::If(v) => v.eval(ctx),
            Self::For(v) => v.eval(ctx),
        }
    }
}

impl Eval for Lit {
    type Output = Value;

    fn eval(&self, _: &mut EvalContext) -> Self::Output {
        match self.kind {
            LitKind::None => Value::None,
            LitKind::Bool(v) => Value::Bool(v),
            LitKind::Int(v) => Value::Int(v),
            LitKind::Float(v) => Value::Float(v),
            LitKind::Length(v, unit) => Value::Length(Length::with_unit(v, unit)),
            LitKind::Angle(v, unit) => Value::Angle(Angle::with_unit(v, unit)),
            LitKind::Percent(v) => Value::Relative(Relative::new(v / 100.0)),
            LitKind::Color(v) => Value::Color(Color::Rgba(v)),
            LitKind::Str(ref v) => Value::Str(v.clone()),
        }
    }
}

impl Eval for ExprArray {
    type Output = ValueArray;

    fn eval(&self, ctx: &mut EvalContext) -> Self::Output {
        self.items.iter().map(|expr| expr.eval(ctx)).collect()
    }
}

impl Eval for ExprDict {
    type Output = ValueDict;

    fn eval(&self, ctx: &mut EvalContext) -> Self::Output {
        self.items
            .iter()
            .map(|Named { name, expr }| (name.string.clone(), expr.eval(ctx)))
            .collect()
    }
}

impl Eval for ExprTemplate {
    type Output = TemplateNode;

    fn eval(&self, ctx: &mut EvalContext) -> Self::Output {
        let tree = Rc::clone(&self.tree);
        let map = self.tree.eval(ctx);
        TemplateNode::Tree { tree, map }
    }
}

impl Eval for ExprGroup {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> Self::Output {
        self.expr.eval(ctx)
    }
}

impl Eval for ExprBlock {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> Self::Output {
        if self.scoping {
            ctx.scopes.push();
        }

        let mut output = Value::None;
        for expr in &self.exprs {
            output = expr.eval(ctx);
        }

        if self.scoping {
            ctx.scopes.pop();
        }

        output
    }
}

impl Eval for ExprUnary {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> Self::Output {
        let value = self.expr.eval(ctx);
        if value == Value::Error {
            return Value::Error;
        }

        let ty = value.type_name();
        let out = match self.op {
            UnOp::Pos => ops::pos(value),
            UnOp::Neg => ops::neg(value),
            UnOp::Not => ops::not(value),
        };

        if out == Value::Error {
            ctx.diag(error!(
                self.span,
                "cannot apply '{}' to {}",
                self.op.as_str(),
                ty,
            ));
        }

        out
    }
}

impl Eval for ExprBinary {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> Self::Output {
        match self.op {
            BinOp::Add => self.apply(ctx, ops::add),
            BinOp::Sub => self.apply(ctx, ops::sub),
            BinOp::Mul => self.apply(ctx, ops::mul),
            BinOp::Div => self.apply(ctx, ops::div),
            BinOp::And => self.apply(ctx, ops::and),
            BinOp::Or => self.apply(ctx, ops::or),
            BinOp::Eq => self.apply(ctx, ops::eq),
            BinOp::Neq => self.apply(ctx, ops::neq),
            BinOp::Lt => self.apply(ctx, ops::lt),
            BinOp::Leq => self.apply(ctx, ops::leq),
            BinOp::Gt => self.apply(ctx, ops::gt),
            BinOp::Geq => self.apply(ctx, ops::geq),
            BinOp::Assign => self.assign(ctx, |_, b| b),
            BinOp::AddAssign => self.assign(ctx, ops::add),
            BinOp::SubAssign => self.assign(ctx, ops::sub),
            BinOp::MulAssign => self.assign(ctx, ops::mul),
            BinOp::DivAssign => self.assign(ctx, ops::div),
        }
    }
}

impl ExprBinary {
    /// Apply a basic binary operation.
    fn apply<F>(&self, ctx: &mut EvalContext, op: F) -> Value
    where
        F: FnOnce(Value, Value) -> Value,
    {
        let lhs = self.lhs.eval(ctx);

        // Short-circuit boolean operations.
        match (self.op, &lhs) {
            (BinOp::And, Value::Bool(false)) => return lhs,
            (BinOp::Or, Value::Bool(true)) => return lhs,
            _ => {}
        }

        let rhs = self.rhs.eval(ctx);

        if lhs == Value::Error || rhs == Value::Error {
            return Value::Error;
        }

        let (l, r) = (lhs.type_name(), rhs.type_name());

        let out = op(lhs, rhs);
        if out == Value::Error {
            self.error(ctx, l, r);
        }

        out
    }

    /// Apply an assignment operation.
    fn assign<F>(&self, ctx: &mut EvalContext, op: F) -> Value
    where
        F: FnOnce(Value, Value) -> Value,
    {
        let rhs = self.rhs.eval(ctx);

        let lhs_span = self.lhs.span();
        let slot = if let Expr::Ident(id) = self.lhs.as_ref() {
            match ctx.scopes.get(id) {
                Some(slot) => slot,
                None => {
                    ctx.diag(error!(lhs_span, "unknown variable"));
                    return Value::Error;
                }
            }
        } else {
            ctx.diag(error!(lhs_span, "cannot assign to this expression"));
            return Value::Error;
        };

        let (constant, err, value) = if let Ok(mut inner) = slot.try_borrow_mut() {
            let lhs = std::mem::take(&mut *inner);
            let types = (lhs.type_name(), rhs.type_name());

            *inner = op(lhs, rhs);
            if *inner == Value::Error {
                (false, Some(types), Value::Error)
            } else {
                (false, None, Value::None)
            }
        } else {
            (true, None, Value::Error)
        };

        if constant {
            ctx.diag(error!(lhs_span, "cannot assign to a constant"));
        }

        if let Some((l, r)) = err {
            self.error(ctx, l, r);
        }

        value
    }

    fn error(&self, ctx: &mut EvalContext, l: &str, r: &str) {
        ctx.diag(error!(self.span, "{}", match self.op {
            BinOp::Add => format!("cannot add {} and {}", l, r),
            BinOp::Sub => format!("cannot subtract {1} from {0}", l, r),
            BinOp::Mul => format!("cannot multiply {} with {}", l, r),
            BinOp::Div => format!("cannot divide {} by {}", l, r),
            _ => format!("cannot apply '{}' to {} and {}", self.op.as_str(), l, r),
        }));
    }
}

impl Eval for ExprCall {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> Self::Output {
        let callee = self.callee.eval(ctx);

        if let Value::Func(func) = callee {
            let func = func.clone();
            let mut args = self.args.eval(ctx);
            let returned = func(ctx, &mut args);
            args.finish(ctx);

            return returned;
        } else if callee != Value::Error {
            ctx.diag(error!(
                self.callee.span(),
                "expected function, found {}",
                callee.type_name(),
            ));
        }

        Value::Error
    }
}

impl Eval for ExprArgs {
    type Output = ValueArgs;

    fn eval(&self, ctx: &mut EvalContext) -> Self::Output {
        let items = self
            .items
            .iter()
            .map(|arg| match arg {
                ExprArg::Pos(expr) => ValueArg {
                    name: None,
                    value: expr.eval(ctx).with_span(expr.span()),
                },
                ExprArg::Named(Named { name, expr }) => ValueArg {
                    name: Some(name.string.clone().with_span(name.span)),
                    value: expr.eval(ctx).with_span(expr.span()),
                },
            })
            .collect();

        ValueArgs { span: self.span, items }
    }
}

impl Eval for ExprLet {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> Self::Output {
        let value = match &self.init {
            Some(expr) => expr.eval(ctx),
            None => Value::None,
        };
        ctx.scopes.def_mut(self.binding.as_str(), value);
        Value::None
    }
}

impl Eval for ExprIf {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> Self::Output {
        let condition = self.condition.eval(ctx);
        if let Value::Bool(boolean) = condition {
            return if boolean {
                self.if_body.eval(ctx)
            } else if let Some(expr) = &self.else_body {
                expr.eval(ctx)
            } else {
                Value::None
            };
        } else if condition != Value::Error {
            ctx.diag(error!(
                self.condition.span(),
                "expected boolean, found {}",
                condition.type_name(),
            ));
        }

        Value::Error
    }
}

impl Eval for ExprFor {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> Self::Output {
        macro_rules! iterate {
            (for ($($binding:ident => $value:ident),*) in $iter:expr) => {{
                let mut output = vec![];

                #[allow(unused_parens)]
                for ($($value),*) in $iter {
                    $(ctx.scopes.def_mut($binding.as_str(), $value);)*

                    if let Value::Template(new) = self.body.eval(ctx) {
                        output.extend(new);
                    }
                }

                Value::Template(output)
            }};
        }

        ctx.scopes.push();

        let iter = self.iter.eval(ctx);
        let value = match (self.pattern.clone(), iter) {
            (ForPattern::Value(v), Value::Str(string)) => {
                iterate!(for (v => value) in string.chars().map(|c| Value::Str(c.into())))
            }
            (ForPattern::Value(v), Value::Array(array)) => {
                iterate!(for (v => value) in array.into_iter())
            }
            (ForPattern::Value(v), Value::Dict(dict)) => {
                iterate!(for (v => value) in dict.into_iter().map(|p| p.1))
            }
            (ForPattern::KeyValue(k, v), Value::Dict(dict)) => {
                iterate!(for (k => key, v => value) in dict.into_iter())
            }

            (ForPattern::KeyValue(_, _), Value::Str(_))
            | (ForPattern::KeyValue(_, _), Value::Array(_)) => {
                ctx.diag(error!(self.pattern.span(), "mismatched pattern"));
                Value::Error
            }

            (_, Value::Error) => Value::Error,
            (_, iter) => {
                ctx.diag(error!(
                    self.iter.span(),
                    "cannot loop over {}",
                    iter.type_name(),
                ));
                Value::Error
            }
        };

        ctx.scopes.pop();
        value
    }
}
