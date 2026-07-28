use crate::media_frame::MediaFrame;
use crate::session::SessionId;
use async_trait::async_trait;

#[async_trait]
pub trait MediaSink: Send + Sync {
    fn session_id(&self) -> SessionId;

    async fn on_frame(&self, frame: MediaFrame) -> anyhow::Result<()>;

    async fn on_media_info(&self, info: crate::media_frame::MediaInfo) -> anyhow::Result<()> {
        let _ = info;
        Ok(())
    }

    async fn close(&self) -> anyhow::Result<()> {
        Ok(())
    }
}
