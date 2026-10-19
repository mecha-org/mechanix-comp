//! `wp_security_context_v1`: sandboxed clients get no privileged globals.

use std::sync::Arc;

use smithay::wayland::security_context::{
    SecurityContext, SecurityContextHandler, SecurityContextListenerSource,
};
use tracing::warn;

use crate::backend::Backend;
use crate::state::{ClientState, State};

impl<BackendData: Backend + 'static> SecurityContextHandler for State<BackendData> {
    fn context_created(&mut self, source: SecurityContextListenerSource, _: SecurityContext) {
        let inserted = self.loop_handle.insert_source(source, |stream, _, state| {
            let data = ClientState {
                restricted: true,
                ..Default::default()
            };
            if let Err(err) = state.display_handle.insert_client(stream, Arc::new(data)) {
                warn!("failed to insert sandboxed client: {err}");
            }
        });
        if let Err(err) = inserted {
            warn!("failed to listen on security context socket: {err}");
        }
    }
}
