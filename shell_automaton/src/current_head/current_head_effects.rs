// Copyright (c) SimpleStaking, Viable Systems and Tezedge Contributors
// SPDX-License-Identifier: MIT

use networking::network_channel::NewCurrentHeadNotification;

use crate::bootstrap::BootstrapInitAction;
use crate::service::actors_service::{ActorsMessageTo, ActorsService};
use crate::service::storage_service::{
    StorageRequestPayload, StorageResponseError, StorageResponseSuccess,
};
use crate::storage::request::{StorageRequestCreateAction, StorageRequestor};
use crate::{Action, ActionWithMeta, Service, Store};

use super::{
    CurrentHeadRehydrateErrorAction, CurrentHeadRehydratePendingAction,
    CurrentHeadRehydrateSuccessAction, CurrentHeadRehydratedAction, CurrentHeadState,
};

pub fn current_head_effects<S>(store: &mut Store<S>, action: &ActionWithMeta)
where
    S: Service,
{
    match &action.action {
        Action::CurrentHeadRehydrateInit(_) => {
            let chain_id = store.state().config.chain_id.clone();
            let level_override = store.state().config.current_head_level_override;
            let storage_req_id = store.state().storage.requests.next_req_id();
            store.dispatch(StorageRequestCreateAction {
                payload: StorageRequestPayload::CurrentHeadGet(chain_id, level_override),
                requestor: StorageRequestor::None,
            });
            store.dispatch(CurrentHeadRehydratePendingAction { storage_req_id });
        }
        Action::StorageResponseReceived(content) => {
            let target_req_id = match &store.state().current_head {
                CurrentHeadState::RehydratePending { storage_req_id, .. } => storage_req_id,
                _ => return,
            };
            if content
                .response
                .req_id
                .filter(|id| id.eq(target_req_id))
                .is_none()
            {
                return;
            }

            match &content.response.result {
                Ok(StorageResponseSuccess::CurrentHeadGetSuccess(head, pred)) => {
                    store.dispatch(CurrentHeadRehydrateSuccessAction {
                        head: head.clone(),
                        head_pred: pred.clone(),
                    });
                }
                Err(StorageResponseError::CurrentHeadGetError(error)) => {
                    store.dispatch(CurrentHeadRehydrateErrorAction {
                        error: error.clone(),
                    });
                }
                _ => {}
            }
        }
        Action::CurrentHeadRehydrateSuccess(_) => {
            store.dispatch(CurrentHeadRehydratedAction {});
        }
        Action::CurrentHeadRehydrated(_) => {
            store.dispatch(BootstrapInitAction {});
            notify_new_current_head(store);
        }
        Action::CurrentHeadUpdate(_) => {
            notify_new_current_head(store);
        }
        _ => {}
    }
}

fn notify_new_current_head<S: Service>(store: &mut Store<S>) {
    let block = match store.state().current_head.get() {
        Some(v) => v.clone().into(),
        None => return,
    };
    let chain_id = store.state().config.chain_id.clone().into();
    let is_bootstrapped = store.state().is_bootstrapped();
    let best_remote_level = store.state().best_remote_level();
    let new_head =
        NewCurrentHeadNotification::new(chain_id, block, is_bootstrapped, best_remote_level);
    store
        .service
        .actors()
        .send(ActorsMessageTo::NewCurrentHead(new_head.into()));
}