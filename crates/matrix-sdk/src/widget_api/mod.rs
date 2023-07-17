

pub mod capabilities;
pub mod driver;

#[allow(missing_debug_implementations)]
pub struct WidgetClient<T> {
    driver: T,
}

impl<T: driver::WidgetDriver> WidgetClient<T> {
    pub fn new(driver: T) -> Self {
        Self {
            driver,
        }
    }
}
