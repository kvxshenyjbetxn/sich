/// Identifier shared by the GUI and daemon compiled in this build.
pub fn build_id() -> &'static str {
    env!("SICH_BUILD_ID")
}
