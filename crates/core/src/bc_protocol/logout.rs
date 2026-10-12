use super::{BcCamera, Result};
use crate::bc::{model::*, xml::*};
use std::sync::atomic::Ordering;
use std::time::Duration;

/// How long [`BcCamera::logout`] waits for the camera to answer
const LOGOUT_REPLY_WAIT: Duration = Duration::from_secs(1);

impl BcCamera {
    /// Logout from the camera, and wait (≤ 1 s) for the camera to answer.
    ///
    /// A camera handles requests in order, so its answer means that it has handled the
    /// logout and everything sent before it. Without waiting, a shutdown that closes the
    /// connection right after the logout can lose it: if the camera is still answering
    /// earlier requests (stream stops), writing those answers to the closed connection
    /// fails and the camera drops the session without reading the logout, which leaves it
    /// awake until its own timeout.
    pub async fn logout(&self) -> Result<()> {
        if self.logged_in.load(Ordering::Relaxed) {
            let credentials = self.get_credentials();
            let connection = self.get_connection();
            let msg_num = self.new_message_num();
            let mut sub_logout = connection.subscribe(MSG_ID_LOGOUT, msg_num).await?;

            let username = credentials.username.clone();
            let password = credentials.password.as_ref().cloned().unwrap_or_default();

            let modern_logout = Bc::new_from_xml(
                BcMeta {
                    msg_id: MSG_ID_LOGOUT,
                    channel_id: self.channel_id,
                    msg_num,
                    stream_type: 0,
                    response_code: 0,
                    class: 0x6414,
                },
                BcXml {
                    login_user: Some(LoginUser {
                        version: xml_ver(),
                        user_name: username,
                        password,
                        user_ver: 1,
                    }),
                    login_net: Some(LoginNet::default()),
                    ..Default::default()
                },
            );

            sub_logout.send(modern_logout).await?;
            self.logged_in.store(false, Ordering::Relaxed);
            // Any answer will do (a camera that never answers costs the wait, no more); an
            // error here means the connection is gone, and there is nothing left to wait for
            match tokio::time::timeout(LOGOUT_REPLY_WAIT, sub_logout.recv()).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => log::debug!("No logout reply: {e:?}"),
                Err(_) => log::debug!("No logout reply within {LOGOUT_REPLY_WAIT:?}"),
            }
        }
        self.logged_in.store(false, Ordering::Relaxed);
        Ok(())
    }
}
