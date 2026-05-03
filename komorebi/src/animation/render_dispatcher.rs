use color_eyre::eyre;

pub trait RenderDispatcher {
    fn get_animation_key(&self) -> String;
    fn pre_render(&self) -> eyre::Result<()>;
    fn render(&self, delta: f64) -> eyre::Result<()>;
    fn post_render(&self) -> eyre::Result<()>;

    /// Called by the animation engine when an in-flight animation is cancelled
    /// before it could complete. Implementors should use this to release any
    /// resources allocated in `pre_render` and bring the underlying window
    /// back to a consistent visible state. Default: no-op.
    fn cleanup_on_cancel(&self) {}
}
