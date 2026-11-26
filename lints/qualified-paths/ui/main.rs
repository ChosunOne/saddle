mod inner {
    pub struct Shape;

    pub fn value() -> usize {
        1
    }
}

use crate::inner::Shape;

fn in_an_expression() -> usize {
    crate::inner::value()
}

fn in_a_type(_shape: &crate::inner::Shape) {}

fn imported() -> usize {
    inner::value()
}

mod deeper {
    pub fn through_super() -> super::inner::Shape {
        super::inner::Shape
    }
}

fn main() {
    let _ = (in_an_expression(), imported(), &Shape);
    let _ = in_a_type(&Shape);
    let _ = deeper::through_super();
}
