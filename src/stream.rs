use std::ops::{Deref, DerefMut};

pub struct MyStream(cpal::Stream);
impl From<cpal::Stream> for MyStream {
    fn from(value: cpal::Stream) -> Self {
        Self(value)
    }
}
impl Deref for MyStream {
    type Target = cpal::Stream;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for MyStream {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
unsafe impl Send for MyStream {}
