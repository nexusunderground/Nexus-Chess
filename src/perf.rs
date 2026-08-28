#[cfg(feature = "tracy")]
#[macro_export]
macro_rules! perf_scope {
    ($name:expr) => {
        let _span = tracy_client::span!($name);
    };
}

#[cfg(not(feature = "tracy"))]
#[macro_export]
macro_rules! perf_scope {
    ($name:expr) => {};
}