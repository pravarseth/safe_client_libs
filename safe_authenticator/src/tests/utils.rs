// Copyright 2018 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use access_container::{fetch_authenticator_entry, put_authenticator_entry};
use client::AuthClient;
use ffi_utils::test_utils::{send_via_user_data, sender_as_user_data};
use ffi_utils::{vec_clone_from_raw_parts, FfiResult, ReprC};
use futures::Future;
use routing::XorName;
use rust_sodium::crypto::secretbox;
use safe_core::crypto::shared_secretbox;
use safe_core::ffi::ipc::req::{
    AuthReq as FfiAuthReq, ContainersReq as FfiContainersReq, ShareMDataReq as FfiShareMDataReq,
};
use safe_core::ffi::ipc::resp::MetadataResponse as FfiUserMetadata;
use safe_core::ipc::req::ContainerPermissions;
use safe_core::ipc::resp::UserMetadata;
use safe_core::ipc::{self, AuthReq, ContainersReq, IpcMsg, IpcReq, Permission, ShareMDataReq};
use safe_core::FutureExt;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::slice;
use std::sync::mpsc;
use std::time::Duration;
use {AuthFuture, Authenticator};

#[derive(Debug)]
pub enum Payload {
    Metadata(Vec<(Option<UserMetadata>, XorName, u64)>),
}

pub type ChannelType = Result<(IpcMsg, Option<Payload>), (i32, Option<IpcMsg>)>;

// Creates a containers request asking for "documents with permission to
// insert", and "videos with all the permissions possible".
pub fn create_containers_req() -> HashMap<String, ContainerPermissions> {
    let mut containers = HashMap::new();
    let _ = containers.insert("_documents".to_owned(), btree_set![Permission::Insert]);
    let _ = containers.insert(
        "_videos".to_owned(),
        btree_set![
            Permission::Read,
            Permission::Insert,
            Permission::Update,
            Permission::Delete,
            Permission::ManagePermissions,
        ],
    );
    containers
}

/// Corrupt an access container entry by overriding its secret key.
pub fn corrupt_container(client: &AuthClient, container_id: &str) -> Box<AuthFuture<()>> {
    trace!("Corrupting access container entry {}...", container_id);

    let c2 = client.clone();
    let container_id = container_id.to_owned();

    fetch_authenticator_entry(client)
        .and_then(move |(version, mut ac_entry)| {
            {
                let entry = unwrap!(ac_entry.get_mut(&container_id));
                entry.enc_info = Some((shared_secretbox::gen_key(), secretbox::gen_nonce()));
            }
            // Update the old entry.
            put_authenticator_entry(&c2, &ac_entry, version + 1)
        }).into_box()
}

// Helper to decode IpcMsg.
// TODO: there should be a public function with a signature like this, and the
//       FFI function `ipc::decode_ipc_msg` should be only wrapper over it.
pub fn decode_ipc_msg(authenticator: &Authenticator, msg: &str) -> ChannelType {
    let (tx, rx) = mpsc::channel::<ChannelType>();

    extern "C" fn auth_cb(user_data: *mut c_void, req_id: u32, req: *const FfiAuthReq) {
        unsafe {
            let req = match AuthReq::clone_from_repr_c(req) {
                Ok(req) => req,
                Err(_) => return send_via_user_data::<ChannelType>(user_data, Err((-2, None))),
            };

            let msg = IpcMsg::Req {
                req_id,
                req: IpcReq::Auth(req),
            };

            send_via_user_data::<ChannelType>(user_data, Ok((msg, None)))
        }
    }

    extern "C" fn containers_cb(user_data: *mut c_void, req_id: u32, req: *const FfiContainersReq) {
        unsafe {
            let req = match ContainersReq::clone_from_repr_c(req) {
                Ok(req) => req,
                Err(_) => return send_via_user_data::<ChannelType>(user_data, Err((-2, None))),
            };

            let msg = IpcMsg::Req {
                req_id,
                req: IpcReq::Containers(req),
            };

            send_via_user_data::<ChannelType>(user_data, Ok((msg, None)))
        }
    }

    extern "C" fn share_mdata_cb(
        user_data: *mut c_void,
        req_id: u32,
        req: *const FfiShareMDataReq,
        ffi_metadatas: *const FfiUserMetadata,
    ) {
        unsafe {
            let req = match ShareMDataReq::clone_from_repr_c(req) {
                Ok(req) => req,
                Err(_) => return send_via_user_data::<ChannelType>(user_data, Err((-2, None))),
            };

            let metadatas: Vec<_> = slice::from_raw_parts(ffi_metadatas, req.mdata.len())
                .iter()
                .map(|ffi_metadata| {
                    (
                        if ffi_metadata.name.is_null() {
                            None
                        } else {
                            Some(unwrap!(UserMetadata::clone_from_repr_c(ffi_metadata)))
                        },
                        XorName(ffi_metadata.xor_name),
                        ffi_metadata.type_tag,
                    )
                }).collect();

            let msg = IpcMsg::Req {
                req_id,
                req: IpcReq::ShareMData(req),
            };

            send_via_user_data::<ChannelType>(
                user_data,
                Ok((msg, Some(Payload::Metadata(metadatas)))),
            )
        }
    }

    let ffi_msg = unwrap!(CString::new(msg));
    let mut ud = Default::default();

    unsafe {
        use ffi::ipc::auth_decode_ipc_msg;
        auth_decode_ipc_msg(
            authenticator,
            ffi_msg.as_ptr(),
            sender_as_user_data(&tx, &mut ud),
            auth_cb,
            containers_cb,
            unregistered_cb,
            share_mdata_cb,
            err_cb,
        );
    };

    let ret = match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(r) => r,
        Err(_) => Err((-1, None)),
    };
    drop(tx);
    ret
}

pub extern "C" fn unregistered_cb(
    user_data: *mut c_void,
    req_id: u32,
    extra_data: *const u8,
    extra_data_len: usize,
) {
    unsafe {
        let msg = IpcMsg::Req {
            req_id,
            req: IpcReq::Unregistered(vec_clone_from_raw_parts(extra_data, extra_data_len)),
        };

        send_via_user_data::<ChannelType>(user_data, Ok((msg, None)))
    }
}

pub extern "C" fn err_cb(user_data: *mut c_void, res: *const FfiResult, response: *const c_char) {
    unsafe {
        let ipc_resp = if response.is_null() {
            None
        } else {
            let response = CStr::from_ptr(response);
            match ipc::decode_msg(unwrap!(response.to_str())) {
                Ok(ipc_resp) => Some(ipc_resp),
                Err(_) => None,
            }
        };

        send_via_user_data::<ChannelType>(user_data, Err(((*res).error_code, ipc_resp)))
    }
}
