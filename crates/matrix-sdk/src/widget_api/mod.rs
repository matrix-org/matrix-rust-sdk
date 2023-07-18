pub mod capabilities;
pub mod driver;
pub mod messages;
pub mod openid;
pub mod widget;

#[allow(missing_debug_implementations)]
pub struct WidgetClient<T> {
    driver: T,
    widget: widget::Widget,
}

impl<T: driver::WidgetDriver> WidgetClient<T> {
    pub fn new(driver: T, widget: widget::Widget) -> Self {
        Self { driver, widget }
    }
}
