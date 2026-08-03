pub const PATH: &str = "/tmp";

pub struct Path;

pub struct Widget {
    value: i32,
}

impl Widget {
    pub fn new(value: i32) -> Widget {
        Widget { value }
    }
}

pub fn build() -> Widget {
    Widget::new(3)
}
