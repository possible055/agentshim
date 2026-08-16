pub mod path;
pub mod process;

#[cfg(not(any(unix, windows)))]
compile_error!("agentshim supports only Windows and Unix hosts");
