extern crate env_proc_macro;

pub const OUT_DIR: &str = env_proc_macro::observed_out_dir!();
