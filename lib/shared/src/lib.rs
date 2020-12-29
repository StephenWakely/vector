#[cfg(feature = "conversion")]
pub mod conversion;

#[cfg(feature = "tokenize")]
pub mod tokenize;

#[cfg(feature = "event")]
pub mod event;
#[cfg(feature = "event")]
pub mod lookup;

pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
pub mod test;

#[cfg(test)]
#[macro_use(quickcheck)]
extern crate quickcheck_macros;
