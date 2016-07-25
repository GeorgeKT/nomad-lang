mod arrays;
mod call;
mod expression;
mod function;
mod nameref;
mod operations;
mod types;

pub use self::arrays::{ArrayLiteral, ArrayInitializer, array_lit, array_init};
pub use self::call::Call;
pub use self::expression::Expression;
pub use self::function::{Function, FunctionSignature};
pub use self::nameref::NameRef;
pub use self::operations::{BinaryOp, UnaryOp, unary_op, bin_op, bin_op2};
pub use self::types::{Type};


fn prefix(level: usize) -> String
{
    let mut s = String::with_capacity(level);
    for _ in 0..level {
        s.push(' ')
    }
    s
}

pub trait TreePrinter
{
    fn print(&self, level: usize);
}
