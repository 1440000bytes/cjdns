//! CryptoAuth

use std::convert::TryFrom;
use std::sync::Arc;

use log::*;
use parking_lot::{Mutex, RwLock, RwLockUpgradableReadGuard, RwLockWriteGuard};
use thiserror::Error;

use crate::bytestring::ByteString;
use crate::crypto::crypto_header::{AuthType, Challenge, CryptoHeader};
use crate::crypto::keys::{IpV6, PrivateKey, PublicKey};
use crate::crypto::random::Random;
use crate::crypto::replay_protector::ReplayProtector;
use crate::crypto::utils::{crypto_hash_sha256, crypto_scalarmult_curve25519_base};
use crate::crypto::wipe::Wipe;
use crate::crypto::zero::IsZero;
use crate::interface::wire::message::Message;
use crate::util::events::EventBase;

use self::types::*;

/// Re-export ugly types from `rtypes` with nice names
mod types {
    pub use crate::rtypes::RTypes_CryptoAuth_State_t as State;
    pub use crate::rtypes::RTypes_CryptoStats_t as CryptoStats;
}

pub struct CryptoAuth {
    pub public_key: PublicKey,

    private_key: PrivateKey,
    users: RwLock<Vec<User>>,
    event_base: EventBase,
    rand: Random,
}

#[derive(Default, Clone)]
struct User {
    /// Double-hash of password for AuthType 1
    password_hash: [u8; Challenge::KEYSIZE],
    /// Hash of username for AuthType 2
    user_name_hash: [u8; Challenge::KEYSIZE],
    secret: [u8; 32],
    login: ByteString,
    restricted_to_ip6: Option<IpV6>,
}

pub struct SessionMut {
    pub her_public_key: PublicKey,

    pub display_name: Option<String>,

    /// Bind this CryptoAuth session to the other node's ip6 address,
    /// any packet advertising a key which doesn't hash to this will be dropped.
    pub her_ip6: IpV6,

    /// After this number of seconds of inactivity,
    /// a connection will be reset to prevent them hanging in a bad state.
    pub reset_after_inactivity_seconds: u32,

    /// If a session is not completely setup, reset it after this many seconds of inactivity.
    pub setup_reset_after_inactivity_seconds: u32,

    /// The shared secret.
    shared_secret: [u8; 32],

    her_temp_pub_key: [u8; 32],

    our_temp_priv_key: [u8; 32],

    our_temp_pub_key: [u8; 32],

    /// A password to use for authing with the other party.
    password: Option<ByteString>,

    /// The login name to auth with the other party.
    login: Option<ByteString>,

    /// The next nonce to use.
    next_nonce: u32,

    /// Used to reset the connection if it's in a bad state (no traffic coming in).
    time_of_last_packet: u32,

    /// The method to use for trying to auth with the server.
    auth_type: AuthType,

    /// True if this node began the conversation.
    is_initiator: bool,

    /// If true and the other end is connecting, do not respond until a valid password is sent.
    require_auth: bool,

    established: bool,
}

pub struct Session {
    session_mut: RwLock<SessionMut>,

    // This has to be briefly locked every packet, it should not contaminate the write lock
    // of the SessionMut so that multiple threads can decrypt at the same time...
    replay_protector: Mutex<ReplayProtector>,

    /// A pointer back to the main CryptoAuth context.
    context: Arc<CryptoAuth>,
}

enum Nonce {
    Hello = 0,
    RepeatHello = 1,
    Key = 2,
    RepeatKey = 3,
    FirstTrafficPacket = 4,
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum AddUserError {
    #[error("Duplicate user '{login:?}'")]
    Duplicate { login: ByteString },
}

/// Keep these numbers same as `cffi::CryptoAuth_DecryptErr`
/// because we return numbers directly.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum DecryptErr {
    /// No errors.
    #[error("NONE")]
    None = 0,

    /// Packet too short.
    #[error("RUNT")]
    Runt = 1,

    /// Received a run message to an un-setup session.
    #[error("NO_SESSION")]
    NoSession = 2,

    #[error("FINAL_HANDSHAKE_FAIL")]
    FinalShakeFail = 3,

    #[error("FAILED_DECRYPT_RUN_MSG")]
    FailedDecryptionRunMsg = 4,

    #[error("KEY_PKT_ESTABLISHED_SESSION")]
    KeyPktEstablishedSession = 5,

    #[error("WRONG_PERM_PUBKEY")]
    WrongPermPubkey = 6,

    /// Only specific IPv6 can connect to this CA session and the request has the wrong one.
    #[error("IP_RESTRICTED")]
    IpRestricted = 7,

    /// Authentication is required and is missing.
    #[error("AUTH_REQUIRED")]
    AuthRequired = 8,

    /// Basically this means the login name doesn't exist, beware of giving this information up.
    #[error("UNRECOGNIZED_AUTH")]
    UnrecognizedAuth = 9,

    /// Key packet received and we are not in a state to accept a key packet.
    #[error("STRAY_KEY")]
    StrayKey = 10,

    #[error("HANDSHAKE_DECRYPT_FAILED")]
    HandshakeDecryptFailed = 11,

    /// Set zero as the temporary public key.
    #[error("WISEGUY")]
    Wiseguy = 12,

    /// Duplicate hello or key packet (same temp key and not a repeat-packet type).
    /// Or repeat key packet with different key than what is known.
    /// Or a repeat hello packet for which we already know the temp key (meaning it is associated
    /// with an existing session) when we are not in a state to accept a repeat hello.
    #[error("INVALID_PACKET")]
    InvalidPacket = 13,

    /// Replay checker could not validate this packet.
    #[error("REPLAY")]
    Replay = 14,

    /// Authenticated decryption failed.
    #[error("DECRYPT")]
    Decrypt = 15,
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum DecryptError {
    #[error("DecryptErr: {0}")]
    DecryptErr(DecryptErr),

    #[error("Internal error: {0}")]
    Internal(&'static str),
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum EncryptError {
    #[error("Internal error: {0}")]
    Internal(&'static str),
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    #[error("PublicKey can not be converted to a valid IPv6 address")]
    BadPublicKey,

    #[error("PublicKey is all zeroes")]
    ZeroPublicKey,
}

/// Works like `assert!()` but returns Internal error instead of panicking.
macro_rules! ensure {
    ($cond:expr, $err_type:tt $(,)?) => {
        if !$cond {
            return Err($err_type::Internal(concat!(
                "Condition failed: `",
                stringify!($cond),
                "`"
            )));
        }
    };
    ($cond:expr, $err_type:tt, $msg:literal $(,)?) => {
        if !$cond {
            return Err($err_type::Internal($msg));
        }
    };
}

impl CryptoAuth {
    const LOG_KEYS: bool = false;

    /// Create a new crypto authenticator.
    ///
    /// If `private_key` is `None` one should be randomly generated.
    pub fn new(private_key: Option<PrivateKey>, event_base: EventBase, rand: Random) -> Self {
        let private_key = private_key.unwrap_or_else(|| PrivateKey::new_random(&rand));

        let public_key = crypto_scalarmult_curve25519_base(&private_key);

        if Self::LOG_KEYS {
            debug!(
                concat!(
                    "Initialized CryptoAuth:\n",
                    "    myPrivateKey={}\n",
                    "     myPublicKey={}\n",
                ),
                debug::hex_key(private_key.raw()),
                debug::hex_key(public_key.raw()),
            );
        }

        let users = RwLock::new(vec![]);

        CryptoAuth {
            public_key,
            private_key,
            users,
            event_base,
            rand,
        }
    }

    /// Associate a password with a user.
    ///
    /// If `ipv6` is not `None`, only allow connections to this CryptoAuth from
    /// the key which hashes to the given IPv6 address.
    pub fn add_user_ipv6(
        &self,
        password: ByteString,
        login: Option<ByteString>,
        ipv6: Option<IpV6>,
    ) -> Result<(), AddUserError> {
        let mut users = self.users.write();
        let mut user = User::default();
        if let Some(login) = login.clone() {
            user.login = login;
        } else {
            user.login = ByteString::from(format!("Anon #{}", users.len()));
        }

        // Users specified with a login field might want to use authType 1 still.
        let (_secret, ac) = hash_password(&user.login, &password, AuthType::Two);
        //user.secret = secret;
        user.user_name_hash.copy_from_slice(ac.as_key_bytes());

        let (secret, ac) = hash_password(&ByteString::empty(), &password, AuthType::One);
        user.secret = secret;
        user.password_hash.copy_from_slice(ac.as_key_bytes());

        for u in &*users {
            if user.secret == u.secret {
                // Do nothing
            } else if let Some(login) = login.as_ref() {
                if *login == u.login {
                    return Err(AddUserError::Duplicate {
                        login: login.clone(),
                    });
                }
            }
        }

        user.restricted_to_ip6 = ipv6;

        users.push(user);

        Ok(())
    }

    /// Remove all users registered with this CryptoAuth.
    ///
    /// If `login` is not `None`, only users with this id will be removed,
    /// otherwise all users will be removed.
    ///
    /// Returns the number of users removed.
    pub fn remove_users(&self, login: Option<ByteString>) -> usize {
        let mut users = self.users.write();
        let mut count = 0;
        users.retain(|u| {
            let remove = login.is_none() || login.as_deref() == Some(&u.login);
            if remove {
                count += 1;
            }
            !remove
        });
        if let Some(login) = login {
            debug!(
                "Removing [{}] user(s) identified by [{}]",
                count,
                login.into_debug_string()
            );
        } else {
            debug!("Flushing [{}] users", count);
        }
        return count;
    }

    /// Get a list of all the users' logins.
    pub fn get_users(&self) -> Vec<ByteString> {
        self.users
            .read()
            .iter()
            .map(|user| user.login.clone())
            .collect()
    }

    /// Search the authorized passwords for one matching this auth header.
    fn get_auth(&self, auth: &Challenge) -> Option<User> {
        if auth.auth_type == AuthType::Zero {
            return None;
        }

        let mut count = 0;

        let users = self.users.read();
        for u in users.iter() {
            count += 1;
            match auth.auth_type {
                AuthType::Zero => unreachable!(),
                AuthType::One => {
                    if *auth.as_key_bytes() == u.password_hash {
                        return Some(u.clone());
                    }
                }
                AuthType::Two => {
                    if *auth.as_key_bytes() == u.user_name_hash {
                        return Some(u.clone());
                    }
                }
            }
        }

        debug!("Got unrecognized auth, password count = [{}]", count);
        None
    }
}

impl SessionMut {
    fn set_auth(&mut self, password: Option<ByteString>, login: Option<ByteString>) {
        if password.is_none() && (self.password.is_some() || self.auth_type != AuthType::Zero) {
            self.password = None;
            self.auth_type = AuthType::Zero;
        } else if self.password.is_none() || self.password != password {
            self.password = password;
            self.auth_type = AuthType::One;
            if login.is_some() {
                self.auth_type = AuthType::Two;
                self.login = login;
            }
        } else {
            return;
        }
        self.reset();
    }

    fn get_state(&self) -> State {
        if self.next_nonce <= State::ReceivedKey as u32 {
            let ret = match self.next_nonce {
                0 => State::Init,
                1 => State::SentHello,
                2 => State::ReceivedHello,
                3 => State::SentKey,
                4 => State::ReceivedKey,
                _ => unreachable!(),
            };
            debug_assert_eq!(self.next_nonce, ret as u32);
            ret
        } else if self.established {
            State::Established
        } else {
            State::ReceivedKey
        }
    }

    fn get_her_pubkey(&self) -> [u8; 32] {
        self.her_public_key.raw().clone()
    }

    fn get_her_ip6(&self) -> [u8; 16] {
        self.her_ip6.raw().clone()
    }

    fn get_name(&self) -> Option<String> {
        self.display_name.clone()
    }

    fn reset_if_timeout(&mut self, event_base: &EventBase) {
        if self.next_nonce == State::SentHello as u32 {
            // Lets not reset the session, we just sent one or more hello packets and
            // have not received a response, if they respond after we reset then we'll
            // be in a tough state.
            return;
        }

        let now_secs = event_base.current_time_seconds() as i64;
        let time_of_last_packet = self.time_of_last_packet as i64;
        let delta = now_secs - time_of_last_packet;
        if delta < self.setup_reset_after_inactivity_seconds as i64 {
            return;
        } else if delta < self.reset_after_inactivity_seconds as i64 {
            if self.established {
                return;
            }
        }

        debug::log(self, || {
            format!("No traffic in [{}] seconds, resetting connection.", delta)
        });
        self.time_of_last_packet = now_secs as u32;
        self.reset();
    }

    /// Does not reset the `replay_protector`
    fn reset(&mut self) {
        self.next_nonce = State::Init as u32;
        self.is_initiator = false;

        self.our_temp_priv_key = [0; 32];
        self.our_temp_pub_key = [0; 32];
        self.her_temp_pub_key = [0; 32];
        self.shared_secret = [0; 32];
        self.established = false;
    }

    fn her_key_known(&self) -> bool {
        !self.her_public_key.is_zero()
    }

    fn encrypt(sess: &Session, msg: &mut Message) -> Result<(), EncryptError> {
        let mut session = sess.session_mut.write();

        // If there has been no incoming traffic for a while, reset the connection to state 0.
        // This will prevent "connection in bad state" situations from lasting forever.
        // This will reset the session if it has timed out.
        session.reset_if_timeout(&sess.context.event_base);

        // If the nonce wraps, start over.
        const MAX_NONCE: u32 = u32::MAX - 0xF;
        if session.next_nonce >= MAX_NONCE {
            session.reset();
        }

        ensure!(msg.is_aligned_to(4), EncryptError, "Alignment fault");

        // next_nonce 0: sending hello, we are initiating connection.
        // next_nonce 1: sending another hello, nothing received yet.
        // next_nonce 2: sending key, hello received.
        // next_nonce 3: sending key again, no data packet received yet.
        // next_nonce >3: handshake complete.
        //
        // If it's a blind handshake, every message will be empty and
        // next_nonce will remain zero until the first message
        // is received back.
        if session.next_nonce <= State::ReceivedKey as u32 {
            if session.next_nonce < State::ReceivedKey as u32 {
                return session.encrypt_handshake(msg, sess.context.clone());
            } else {
                debug::log(&session, || "Doing final step to send message. nonce=4");
                debug_assert!(!session.our_temp_priv_key.is_zero());
                debug_assert!(!session.her_temp_pub_key.is_zero());
                session.shared_secret =
                    get_shared_secret(session.our_temp_priv_key, session.her_temp_pub_key, None);
            }
        }

        ensure!(msg.len() > 0, EncryptError, "Empty packet during handshake");
        ensure!(msg.pad() >= 36, EncryptError, "Not enough padding");

        let session = RwLockWriteGuard::downgrade_to_upgradable(session);

        encrypt(
            session.next_nonce,
            msg,
            session.shared_secret.clone(),
            session.is_initiator,
        );

        let mut session = RwLockUpgradableReadGuard::upgrade(session);

        let r = msg.push(session.next_nonce.to_be()); // Big-endian push
        ensure!(r.is_ok(), EncryptError, "push nonce failed");
        session.next_nonce += 1;
        Ok(())
    }

    fn decrypt(sess: &Session, msg: &mut Message) -> Result<(), DecryptError> {
        let session = sess.session_mut.upgradable_read();

        if msg.len() < 20 {
            debug::log(&session, || "DROP runt");
            return Err(DecryptError::DecryptErr(DecryptErr::Runt));
        }

        ensure!(
            msg.pad() >= 12,
            DecryptError,
            "Need at least 12 bytes of padding in incoming message"
        );
        ensure!(msg.is_aligned_to(4), DecryptError, "Alignment fault");
        ensure!(msg.cap() % 4 == 0, DecryptError, "Length fault");

        ensure!(msg.len() >= CryptoHeader::SIZE, DecryptError);
        let header = msg.peek::<CryptoHeader>().unwrap().clone();

        debug_assert!(msg.len() >= 4); // Due to the check in the beginning
        let state = msg.pop::<u32>().expect("pop 4 bytes"); // Safe

        let nonce = header.nonce.to_be(); // Read as Big-Endian

        if !session.established {
            if nonce >= Nonce::FirstTrafficPacket as u32 {
                if session.next_nonce < State::SentKey as u32 {
                    // This is impossible because we have not exchanged hello and key messages.
                    debug::log(&session, || {
                        "DROP Received a run message to an un-setup session"
                    });
                    return Err(DecryptError::DecryptErr(DecryptErr::NoSession));
                }

                debug::log(&session, || {
                    format!("Trying final handshake step, nonce={}\n", nonce)
                });

                debug_assert!(!session.our_temp_priv_key.is_zero());
                debug_assert!(!session.her_temp_pub_key.is_zero());

                let secret =
                    get_shared_secret(session.our_temp_priv_key, session.her_temp_pub_key, None);

                let ret = session.decrypt_message(nonce, msg, secret, sess);

                // This prevents a few "ghost" dropped packets at the beginning of a session.
                sess.replay_protector.lock().init(nonce + 1);

                if ret.is_ok() {
                    let mut session = RwLockUpgradableReadGuard::upgrade(session);

                    debug::log(&session, || "Final handshake step succeeded");
                    session.shared_secret = secret;

                    // Now we're in run mode, no more handshake packets will be accepted
                    session.established = true;
                    session.next_nonce += 3;
                    session.update_time(msg, sess.context.clone());
                    return Ok(());
                }
                debug::log(&session, || "DROP Final handshake step failed");
                ret
            } else {
                msg.push(state).expect("push state back");

                let mut session = RwLockUpgradableReadGuard::upgrade(session);

                session.decrypt_handshake(nonce, msg, header, sess)
            }
        } else if nonce >= Nonce::FirstTrafficPacket as u32 {
            debug_assert!(!session.shared_secret.is_zero());

            let ret = session.decrypt_message(nonce, msg, session.shared_secret.clone(), sess);
            match ret {
                Ok(_) => {
                    let mut session = RwLockUpgradableReadGuard::upgrade(session);

                    session.update_time(msg, sess.context.clone());
                    Ok(())
                }
                Err(err) => {
                    debug::log(&session, || {
                        format!(
                            "DROP Failed to [{}] message",
                            if err == DecryptError::DecryptErr(DecryptErr::Replay) {
                                "replay check"
                            } else {
                                "decrypt"
                            },
                        )
                    });
                    Err(err)
                }
            }
        } else if nonce <= Nonce::RepeatHello as u32 {
            let mut session = RwLockUpgradableReadGuard::upgrade(session);

            debug::log(&session, || {
                format!("hello packet during established session nonce=[{}]", nonce)
            });
            msg.push(state).expect("push state back");
            session.decrypt_handshake(nonce, msg, header, sess)
        } else {
            debug::log(&session, || {
                format!(
                    "DROP key packet during established session nonce=[{}]",
                    nonce
                )
            });
            Err(DecryptError::DecryptErr(
                DecryptErr::KeyPktEstablishedSession,
            ))
        }
    }

    fn encrypt_handshake(
        &mut self,
        msg: &mut Message,
        context: Arc<CryptoAuth>,
    ) -> Result<(), EncryptError> {
        // Prepend message with a CryptoHeader struct
        let r = msg.push(CryptoHeader::default());
        ensure!(r.is_ok(), EncryptError, "push CryptoHeader failed");

        // Garbage the auth challenge and set the nonce which follows it
        {
            // Get the just added header as byte slice
            let header = msg.peek_bytes_mut(CryptoHeader::SIZE).unwrap();
            // Offset of the `auth` field in `CryptoHeader` struct
            const OFFS: usize = 4;
            // Total size of the `auth` and `handshake_nonce` fields
            const LEN: usize = Challenge::SIZE + 24;
            let dest = &mut header[OFFS..(OFFS + LEN)];
            context.rand.random_bytes(dest);
        }

        // Get inplace mutable CryptoHeader ref inside message
        let header = msg.peek_mut::<CryptoHeader>().unwrap();

        // Set the permanent key
        header.public_key = context.public_key.raw().clone();

        ensure!(self.her_key_known(), EncryptError);

        // Password auth
        let password_hash;
        if let (Some(login), Some(password)) = (self.login.as_ref(), self.password.as_ref()) {
            let (pwd_hash, auth) = hash_password(&*login, &*password, self.auth_type);
            header.auth = auth;
            password_hash = Some(pwd_hash);
        } else {
            header.auth.auth_type = self.auth_type;
            header.auth.additional = 0;
            password_hash = None;
        }

        // Set the session state
        header.nonce = self.next_nonce.to_be(); // Big-endian nonce

        if self.next_nonce == State::Init as u32 || self.next_nonce == State::ReceivedHello as u32 {
            // If we're sending a hello or a key
            // Here we make up a temp keypair
            context.rand.random_bytes(&mut self.our_temp_priv_key);
            self.our_temp_pub_key = {
                //TODO Likely to be simplified after using proper types everywhere
                let priv_key = PrivateKey::from(self.our_temp_priv_key.clone());
                let pub_key = crypto_scalarmult_curve25519_base(&priv_key);
                pub_key.raw().clone()
            };

            if CryptoAuth::LOG_KEYS {
                debug!(
                    concat!(
                        "Generating temporary keypair\n",
                        "    myTempPrivateKey={}\n",
                        "     myTempPublicKey={}\n",
                    ),
                    hex::encode(&self.our_temp_priv_key),
                    hex::encode(&self.our_temp_pub_key),
                );
            }
        }

        header.encrypted_temp_key = self.our_temp_pub_key;

        if CryptoAuth::LOG_KEYS {
            debug!(
                concat!("Wrapping temp public key:\n", "    {}\n",),
                hex::encode(&header.encrypted_temp_key),
            );
        }

        debug::log(self, || {
            format!(
                "Sending {}{} packet (auth: {})",
                if self.next_nonce & 1 != 0 {
                    "repeat "
                } else {
                    ""
                },
                if self.next_nonce < State::ReceivedHello as u32 {
                    "hello"
                } else {
                    "key"
                },
                password_hash.is_some(),
            )
        });

        let shared_secret;
        if self.next_nonce < State::ReceivedHello as u32 {
            shared_secret = get_shared_secret(
                context.private_key.raw().clone(),
                self.her_public_key.raw().clone(),
                password_hash,
            );

            self.is_initiator = true;

            ensure!(self.next_nonce <= State::SentHello as u32, EncryptError);
            self.next_nonce = State::SentHello as u32;
        } else {
            // Handshake2
            // her_temp_pub_key was set by decrypt_handshake()
            debug_assert!(!self.her_temp_pub_key.is_zero());
            shared_secret = get_shared_secret(
                context.private_key.raw().clone(),
                self.her_temp_pub_key,
                password_hash,
            );

            ensure!(self.next_nonce <= State::SentKey as u32, EncryptError);
            self.next_nonce = State::SentKey as u32;

            if CryptoAuth::LOG_KEYS {
                debug!(
                    concat!("Using their temp public key:\n", "    {}\n",),
                    hex::encode(&self.her_temp_pub_key),
                );
            }
        }

        ensure!(
            (self.next_nonce < State::ReceivedHello as u32) == self.her_temp_pub_key.is_zero(),
            EncryptError,
        );

        let handshake_nonce = header.handshake_nonce;

        // Temporarily remove CryptoHeader until the encrypted_temp_key field.
        let mut saved = msg.pop_bytes(CryptoHeader::SIZE - 32).expect("pop");

        encrypt_rnd_nonce(handshake_nonce.clone(), msg, shared_secret);

        if CryptoAuth::LOG_KEYS {
            debug!(
                concat!(
                    "Encrypting message with:\n",
                    "    nonce: {}\n",
                    "   secret: {}\n",
                    "   cipher: {}\n",
                ),
                hex::encode(handshake_nonce),
                debug::hex_key(&shared_secret),
                hex::encode(&msg.bytes()[0..32]), //TODO correct? no?
            );
        }

        // Put CryptoHeader back (without last 16 bytes,
        // because encrypt_rnd_nonce adds these 16 bytes of authenticator).
        saved.truncate(saved.len() - 16);
        msg.push_bytes(&saved).expect("push");

        Ok(())
    }

    fn decrypt_handshake(
        &mut self,
        nonce: u32,
        msg: &mut Message,
        header: CryptoHeader,
        sess: &Session,
    ) -> Result<(), DecryptError> {
        if msg.len() < CryptoHeader::SIZE {
            debug::log(self, || "DROP runt");
            return Err(DecryptError::DecryptErr(DecryptErr::Runt));
        }

        // handshake
        // next_nonce 0: receiving hello.
        // next_nonce 1: receiving key, we sent hello.
        // next_nonce 2: receiving first data packet or duplicate hello.
        // next_nonce 3: receiving first data packet.
        // next_nonce >3: handshake complete

        ensure!(self.her_key_known(), DecryptError);
        if *self.her_public_key.raw() != header.public_key {
            debug::log(self, || {
                "DROP a packet with different public key than this session"
            });
            return Err(DecryptError::DecryptErr(DecryptErr::WrongPermPubkey));
        }

        ensure!(
            (self.next_nonce < State::ReceivedHello as u32) == self.her_temp_pub_key.is_zero(),
            DecryptError,
        );

        let user_opt = sess.context.get_auth(&header.auth);
        let has_user = user_opt.is_some();

        let password_hash;

        if let Some(user) = user_opt {
            password_hash = Some(user.secret);
            let restricted_to_ip6 = user.restricted_to_ip6;
            if restricted_to_ip6.is_some() {
                let ip6_matches_key = {
                    let pub_key = &self.her_public_key;
                    let calculated_ip6 = IpV6::try_from(pub_key).ok();
                    restricted_to_ip6 == calculated_ip6
                };
                if !ip6_matches_key {
                    debug::log(self, || "DROP packet with key not matching restrictedToIp6");
                    return Err(DecryptError::DecryptErr(DecryptErr::IpRestricted));
                }
            }
        } else {
            password_hash = None;
        }

        if self.require_auth && !has_user {
            debug::log(self, || "DROP message because auth was not given");
            return Err(DecryptError::DecryptErr(DecryptErr::AuthRequired));
        }

        if !has_user && header.auth.auth_type != AuthType::Zero {
            debug::log(self, || "DROP message with unrecognized authenticator");
            return Err(DecryptError::DecryptErr(DecryptErr::UnrecognizedAuth));
        }

        // What the nextNonce will become if this packet is valid.
        let mut next_nonce: u32;

        // The secret for decrypting this message.
        let shared_secret: [u8; 32];

        if nonce < Nonce::Key as u32 {
            // Nonce::Hello or Nonce::RepeatHello
            debug::log(self, || {
                format!(
                    "Received a {}hello packet, using auth: {}",
                    if nonce == Nonce::RepeatHello as u32 {
                        "repeat"
                    } else {
                        ""
                    },
                    has_user,
                )
            });

            shared_secret = get_shared_secret(
                sess.context.private_key.raw().clone(),
                self.her_public_key.raw().clone(),
                password_hash,
            );

            next_nonce = State::ReceivedHello as u32;
        } else {
            if nonce == Nonce::Key as u32 {
                debug::log(self, || "Received a key packet");
            } else {
                ensure!(nonce == Nonce::RepeatKey as u32, DecryptError);
                debug::log(self, || "Received a repeat key packet");
            }

            if !self.is_initiator {
                debug::log(self, || "DROP a stray key packet");
                return Err(DecryptError::DecryptErr(DecryptErr::StrayKey));
            }

            // We sent the hello, this is a key
            shared_secret = get_shared_secret(
                self.our_temp_priv_key.clone(),
                self.her_public_key.raw().clone(),
                password_hash,
            );

            next_nonce = State::ReceivedKey as u32;
        }

        // Shift it on top of the authenticator before the encrypted public key
        msg.discard_bytes(CryptoHeader::SIZE - 48)
            .expect("discard above authenticator");

        if CryptoAuth::LOG_KEYS {
            debug!(
                concat!(
                    "Decrypting message with:\n",
                    "    nonce: {}\n",
                    "   secret: {}\n",
                    "   cipher: {}\n",
                ),
                hex::encode(&header.handshake_nonce),
                debug::hex_key(&shared_secret),
                hex::encode(&msg.bytes()[0..32]), //TODO correct? no?
            );
        }

        // Decrypt her temp public key and the message.
        let r = decrypt_rnd_nonce(header.handshake_nonce.clone(), msg, shared_secret);
        if r.is_err() {
            header.wipe(); // Just in case
            debug::log(self, || {
                format!("DROP message with nonce [{}], decryption failed", nonce)
            });
            return Err(DecryptError::DecryptErr(DecryptErr::HandshakeDecryptFailed));
        }

        if header.encrypted_temp_key.is_zero() {
            // We need to reject 0 public keys outright because they will be confused with "unknown"
            debug::log(self, || "DROP message with zero as temp public key");
            return Err(DecryptError::DecryptErr(DecryptErr::Wiseguy));
        }

        if CryptoAuth::LOG_KEYS {
            debug!(
                concat!("Unwrapping temp public key:\n", "    {}\n",),
                hex::encode(&header.encrypted_temp_key),
            );
        }

        msg.discard_bytes(32).expect("discard 32 bytes");

        // Post-decryption checking
        if nonce == Nonce::Hello as u32 {
            // A new hello packet
            if self.her_temp_pub_key == header.encrypted_temp_key {
                // Possible replay attack or duped packet
                debug::log(self, || "DROP dupe hello packet with same temp key");
                return Err(DecryptError::DecryptErr(DecryptErr::InvalidPacket));
            }
        } else if nonce == Nonce::Key as u32 && self.next_nonce >= State::ReceivedKey as u32 {
            // We accept a new key packet and let it change the session since the other end might have
            // killed off the session while it was in the midst of setting up.
            // This is NOT a repeat key packet because it's nonce is 2, not 3.
            if self.her_temp_pub_key == header.encrypted_temp_key {
                ensure!(!self.her_temp_pub_key.is_zero(), DecryptError);
                debug::log(self, || "DROP dupe key packet with same temp key");
                return Err(DecryptError::DecryptErr(DecryptErr::InvalidPacket));
            }
        } else if nonce == Nonce::RepeatKey as u32 && self.next_nonce >= State::ReceivedKey as u32 {
            // Got a repeat key packet, make sure the temp key is the same as the one we know.
            if self.her_temp_pub_key != header.encrypted_temp_key {
                ensure!(!self.her_temp_pub_key.is_zero(), DecryptError);
                debug::log(self, || "DROP repeat key packet with different temp key");
                return Err(DecryptError::DecryptErr(DecryptErr::InvalidPacket));
            }
        }

        // Consts for the match clauses below
        const INIT: u32 = State::Init as u32;
        const RECEIVED_HELLO: u32 = State::ReceivedHello as u32;
        const SENT_KEY: u32 = State::SentKey as u32;
        const SENT_HELLO: u32 = State::SentHello as u32;
        const RECEIVED_KEY: u32 = State::ReceivedKey as u32;

        // If Alice sent a hello packet then Bob sent a hello packet and they crossed on the wire,
        // somebody has to yield and the other has to stand firm otherwise they will either deadlock
        // each believing their hello packet is superior or they will livelock, each switching to the
        // other's session and never synchronizing.
        // In this event whoever has the lower permanent public key wins.

        // If we receive a (possibly repeat) key packet
        if next_nonce == State::ReceivedKey as u32 {
            ensure!(
                nonce == Nonce::Key as u32 || nonce == Nonce::RepeatKey as u32,
                DecryptError
            );
            match self.next_nonce {
                INIT | RECEIVED_HELLO | SENT_KEY => {
                    debug::log(self, || "DROP stray key packet");
                    return Err(DecryptError::DecryptErr(DecryptErr::StrayKey));
                }
                SENT_HELLO => {
                    self.her_temp_pub_key = header.encrypted_temp_key;
                }
                RECEIVED_KEY => {
                    if nonce == Nonce::Key as u32 {
                        self.her_temp_pub_key = header.encrypted_temp_key;
                    } else {
                        ensure!(
                            self.her_temp_pub_key == header.encrypted_temp_key,
                            DecryptError
                        );
                    }
                }
                _ => {
                    ensure!(!self.established, DecryptError);
                    if nonce == Nonce::Key as u32 {
                        self.her_temp_pub_key = header.encrypted_temp_key;
                        debug::log(self, || "New key packet, recalculating shared secret");

                        debug_assert!(!self.our_temp_priv_key.is_zero());
                        debug_assert!(!self.her_temp_pub_key.is_zero());

                        self.shared_secret =
                            get_shared_secret(self.our_temp_priv_key, self.her_temp_pub_key, None);
                    } else {
                        ensure!(
                            self.her_temp_pub_key == header.encrypted_temp_key,
                            DecryptError
                        );
                    }

                    next_nonce = self.next_nonce + 1;
                    debug::log(self, || "New key packet but we are already sending data");
                }
            }
        } else if next_nonce == State::ReceivedHello as u32 {
            ensure!(
                nonce == Nonce::Hello as u32 || nonce == Nonce::RepeatHello as u32,
                DecryptError
            );
            if self.her_temp_pub_key != header.encrypted_temp_key {
                // Fresh new hello packet, we should reset the session.
                match self.next_nonce {
                    SENT_HELLO => {
                        if self.her_public_key < sess.context.public_key {
                            // It's a hello and we are the initiator but their permanent public key is
                            // numerically lower than ours, this is so that in the event of two hello
                            // packets crossing on the wire, the nodes will agree on who is the
                            // initiator.
                            debug::log(self, || {
                                "Incoming hello from node with lower key, resetting"
                            });
                            self.reset();
                            sess.replay_protector.lock().reset();
                            self.her_temp_pub_key = header.encrypted_temp_key;
                        } else {
                            // We are the initiator and thus we are sending HELLO packets, however they
                            // have sent a hello to us and we already sent a HELLO
                            // We accept the packet (return 0) but we do not alter the state because
                            // we have our own state and we will respond with our (key) packet.
                            debug::log(self, || {
                                "Incoming hello from node with higher key, not resetting"
                            });
                            return Ok(());
                        }
                    }
                    INIT => {
                        self.her_temp_pub_key = header.encrypted_temp_key;
                    }
                    _ => {
                        debug::log(self, || "Incoming hello packet resetting session");
                        self.reset();
                        sess.replay_protector.lock().reset();
                        self.her_temp_pub_key = header.encrypted_temp_key;
                    }
                }
            } else {
                // Received a hello packet with the same key as the session we already know...
                match self.next_nonce {
                    RECEIVED_HELLO | SENT_KEY => {
                        next_nonce = self.next_nonce;
                    }
                    _ => {
                        debug::log(self, || "DROP Incoming repeat hello");
                        // We already know the key which is being used for this hello packet and
                        // our state has advanced past RECEIVED_HELLO or SENT_KEY or perhaps we
                        // are the initiator of this session and they're sending us what should
                        // be a key packet but is marked as hello, it's all invalid.
                        return Err(DecryptError::DecryptErr(DecryptErr::InvalidPacket));
                    }
                }
            }
        } else {
            unreachable!("should never happen");
        }

        // Nonce can never go backward and can only "not advance" if they're 0,1,2,3,4 session state.
        ensure!(
            self.next_nonce < next_nonce
                || (self.next_nonce <= State::ReceivedKey as u32 && next_nonce == self.next_nonce),
            DecryptError,
            "nonce sequence error",
        );
        self.next_nonce = next_nonce;

        sess.replay_protector.lock().reset();

        Ok(())
    }

    #[inline]
    fn decrypt_message(
        &self,
        nonce: u32,
        content: &mut Message,
        secret: [u8; 32],
        sess: &Session,
    ) -> Result<(), DecryptError> {
        // Decrypt with authentication and replay prevention.
        let r = decrypt(nonce, content, secret, self.is_initiator);
        if r.is_err() {
            debug::log(self, || "DROP authenticated decryption failed");
            return Err(DecryptError::DecryptErr(DecryptErr::Decrypt));
        }

        if !sess.replay_protector.lock().check_nonce(nonce) {
            debug::log(self, || {
                format!("DROP nonce checking failed nonce=[{}]", nonce)
            });
            return Err(DecryptError::DecryptErr(DecryptErr::Replay));
        }

        Ok(())
    }

    #[inline]
    fn update_time(&mut self, _msg: &Message, context: Arc<CryptoAuth>) {
        self.time_of_last_packet = context.event_base.current_time_seconds();
    }
}

impl Session {
    const DEFAULT_RESET_AFTER_INACTIVITY_SECONDS: u32 = 60;
    const DEFAULT_SETUP_RESET_AFTER_INACTIVITY_SECONDS: u32 = 10;

    pub fn new(
        context: Arc<CryptoAuth>,
        her_pub_key: PublicKey,
        require_auth: bool,
        display_name: Option<String>,
        use_noise: bool,
    ) -> Result<Self, KeyError> {
        let now = context.event_base.current_time_seconds();

        if use_noise {
            unimplemented!("noise protocol");
        }

        if her_pub_key.is_zero() {
            return Err(KeyError::ZeroPublicKey);
        }
        let her_ip6 = IpV6::try_from(&her_pub_key).map_err(|_| KeyError::BadPublicKey)?;

        let sess = Session {
            session_mut: RwLock::new(SessionMut {
                her_public_key: her_pub_key,
                display_name,
                her_ip6,
                reset_after_inactivity_seconds: Self::DEFAULT_RESET_AFTER_INACTIVITY_SECONDS,
                setup_reset_after_inactivity_seconds:
                    Self::DEFAULT_SETUP_RESET_AFTER_INACTIVITY_SECONDS,
                shared_secret: [0; 32],
                her_temp_pub_key: [0; 32],
                our_temp_priv_key: [0; 32],
                our_temp_pub_key: [0; 32],
                password: None,
                login: None,
                next_nonce: State::Init as u32,
                time_of_last_packet: now,
                auth_type: AuthType::Zero,
                is_initiator: false,
                require_auth,
                established: false,
            }),
            replay_protector: Mutex::new(ReplayProtector::new()),
            context,
        };

        Ok(sess)
    }

    pub fn set_auth(&self, password: Option<ByteString>, login: Option<ByteString>) {
        self.session_mut.write().set_auth(password, login)
    }

    pub fn get_state(&self) -> State {
        self.session_mut.read().get_state()
    }

    pub fn get_her_pubkey(&self) -> [u8; 32] {
        self.session_mut.read().get_her_pubkey()
    }

    pub fn get_her_ip6(&self) -> [u8; 16] {
        self.session_mut.read().get_her_ip6()
    }

    pub fn get_name(&self) -> Option<String> {
        self.session_mut.read().get_name()
    }

    pub fn stats(&self) -> CryptoStats {
        // Stats come from the replay protector
        let rp = self.replay_protector.lock();
        let stats = rp.stats();
        CryptoStats {
            lost_packets: stats.lost_packets as u64,
            received_unexpected: stats.received_unexpected as u64,
            received_packets: stats.received_packets as u64,
            duplicate_packets: stats.duplicate_packets as u64,
        }
    }

    pub fn reset_if_timeout(&self) {
        self.session_mut
            .write()
            .reset_if_timeout(&self.context.event_base)
    }

    pub fn reset(&self) {
        // Make sure we're write() session_mut when we do the replay because
        // decrypt threads will read() session_mut
        let mut session_mut = self.session_mut.write();
        let mut replay_protector = self.replay_protector.lock();
        replay_protector.reset();
        session_mut.reset();
    }

    pub fn her_key_known(&self) -> bool {
        self.session_mut.read().her_key_known()
    }

    pub fn encrypt(&self, msg: &mut Message) -> Result<(), EncryptError> {
        SessionMut::encrypt(self, msg)
    }

    pub fn decrypt(&self, msg: &mut Message) -> Result<(), DecryptError> {
        SessionMut::decrypt(self, msg)
    }
}

/// Get a shared secret.
///
/// Arg `password_hash` is a 32 byte value known to both ends,
/// this must be provably pseudorandom:
/// the first 32 bytes of a sha256 output from hashing a password is ok,
/// whatever she happens to send me in the `auth` field is NOT ok.
/// If this field is `None`, the secret will be generated without the password.
#[inline]
fn get_shared_secret(
    my_private_key: [u8; 32],
    her_public_key: [u8; 32],
    password_hash: Option<[u8; 32]>,
) -> [u8; 32] {
    let output_secret = if let Some(password_hash) = password_hash {
        use sodiumoxide::crypto::scalarmult::curve25519::*;

        #[repr(C)]
        #[derive(Copy, Clone)]
        struct Components {
            key: [u8; 32],
            passwd: [u8; 32],
        }

        #[repr(C)]
        union Buff {
            components: Components,
            bytes: [u8; 64],
        }

        let buff = Buff {
            components: Components {
                key: {
                    let n = Scalar(my_private_key);
                    let p = GroupElement(her_public_key);
                    scalarmult(&n, &p)
                        .expect("crypto::scalarmult::curve25519")
                        .0
                },
                passwd: { password_hash },
            },
        };

        let bytes = unsafe { buff.bytes };
        crypto_hash_sha256(&bytes)
    } else {
        use sodiumoxide::crypto::box_::curve25519xsalsa20poly1305::*;

        let pk = PublicKey(her_public_key);
        let sk = SecretKey(my_private_key);

        let key = precompute(&pk, &sk);
        key.0
    };

    if CryptoAuth::LOG_KEYS {
        debug!(
            concat!(
                "Generated a shared secret:\n",
                "     myPublicKey={}\n",
                "    herPublicKey={}\n",
                "    passwordHash={}\n",
                "    outputSecret={}\n",
            ),
            debug::hex_pub_key(&my_private_key),
            debug::hex_key(&her_public_key),
            debug::hex_key_opt(password_hash.as_ref()),
            debug::hex_key(&output_secret),
        );
    }

    output_secret
}

#[inline]
fn hash_password(login: &[u8], password: &[u8], auth_type: AuthType) -> ([u8; 32], Challenge) {
    let secret_out = crypto_hash_sha256(password);

    let tmp_buf = match auth_type {
        AuthType::Zero => panic!("Unsupported auth type [{}]", auth_type as u8),
        AuthType::One => crypto_hash_sha256(&secret_out),
        AuthType::Two => crypto_hash_sha256(login),
    };

    let mut challenge_out = Challenge {
        auth_type,
        lookup: [0; 7],
        require_packet_auth_and_derivation_count: 0,
        additional: 0,
    };
    challenge_out.lookup.copy_from_slice(&tmp_buf[1..8]);

    (secret_out, challenge_out)
}

/// Encrypt a packet.
#[inline]
fn encrypt(nonce: u32, msg: &mut Message, secret: [u8; 32], is_initiator: bool) {
    #[repr(C)]
    union Nonce {
        ints: [u32; 2],
        bytes: [u8; 24],
    }
    let offs = if is_initiator { 1 } else { 0 };
    let nonce_bytes = unsafe {
        let mut nonce_as = Nonce { bytes: [0; 24] };
        nonce_as.ints[offs] = nonce.to_le(); // Little-endian nonce
        nonce_as.bytes
    };
    encrypt_rnd_nonce(nonce_bytes, msg, secret);
}

/// Decrypt a packet.
#[inline]
fn decrypt(nonce: u32, msg: &mut Message, secret: [u8; 32], is_initiator: bool) -> Result<(), ()> {
    #[repr(C)]
    union Nonce {
        ints: [u32; 2],
        bytes: [u8; 24],
    }
    let offs = if is_initiator { 0 } else { 1 };
    let nonce_bytes = unsafe {
        let mut nonce_as = Nonce { bytes: [0; 24] };
        nonce_as.ints[offs] = nonce.to_le(); // Little-endian nonce
        nonce_as.bytes
    };
    decrypt_rnd_nonce(nonce_bytes, msg, secret)
}

/// Encrypt and authenticate.
/// Grows the message by 16 bytes.
#[inline]
fn encrypt_rnd_nonce(nonce: [u8; 24], msg: &mut Message, secret: [u8; 32]) {
    //msg.push_bytes(&[0; 32]).expect("pad >= 32");

    {
        use sodiumoxide::crypto::box_::curve25519xsalsa20poly1305::*;
        let bytes = msg.bytes_mut();
        let nonce = Nonce(nonce);
        let key = PrecomputedKey(secret);
        //TODO this data copying is suboptimal. Need proper fn binding.
        let encrypted = seal_precomputed(bytes, &nonce, &key); // adds 16 bytes
        msg.push_bytes(&[0; 16]).expect("pad >= 16"); // also grow orig msg
        let dest = msg.bytes_mut();
        assert_eq!(dest.len(), encrypted.len());
        dest.copy_from_slice(&encrypted);
    }

    // Pop 16 bytes despite we pushed 32
    //msg.discard_bytes(16).expect("discard");
}

/// Decrypt and authenticate.
/// Shrinks the message by 16 bytes.
#[inline]
fn decrypt_rnd_nonce(nonce: [u8; 24], msg: &mut Message, secret: [u8; 32]) -> Result<(), ()> {
    if msg.len() < 16 {
        return Err(());
    }

    //msg.push_bytes(&[0; 16]).expect("pad >= 16");

    {
        use sodiumoxide::crypto::box_::curve25519xsalsa20poly1305::*;
        let bytes = msg.bytes_mut();
        let nonce = Nonce(nonce);
        let key = PrecomputedKey(secret);
        //TODO this data copying is suboptimal. Need proper fn binding.
        let decrypted = open_precomputed(bytes, &nonce, &key)?; // 16 bytes less
        msg.discard_bytes(16).expect("discard 16 bytes"); // Also shrink msg
        let dest = msg.bytes_mut();
        assert_eq!(dest.len(), decrypted.len());
        dest.copy_from_slice(&decrypted);
    }

    //msg.discard_bytes(16).expect("discard");

    Ok(())
}

mod debug {
    use std::convert::TryFrom;

    use cjdns_keys::{IpV6, PrivateKey};

    use crate::crypto::utils::crypto_scalarmult_curve25519_base;
    use crate::crypto::zero::IsZero;

    use super::SessionMut;

    #[inline]
    pub(super) fn log<S, F>(session: &SessionMut, msg: F)
    where
        S: AsRef<str>,
        F: FnOnce() -> S,
    {
        if log::log_enabled!(log::Level::Debug) {
            let sess_ptr = session as *const SessionMut;
            let dn = session.display_name.as_deref().unwrap_or("");
            let addr = get_ip6(session);
            let msg = msg();
            log::debug!(
                "{:?} {} [{}] state[{}]: {}",
                sess_ptr,
                dn,
                addr,
                session.next_nonce,
                msg.as_ref()
            );
        }
    }

    #[inline]
    fn get_ip6(session: &SessionMut) -> String {
        assert!(session.her_key_known());
        match IpV6::try_from(&session.her_public_key) {
            Ok(ipv6) => ipv6.to_string(),
            Err(e) => e.to_string(),
        }
    }

    #[inline]
    pub(super) fn hex_key(key: &[u8; 32]) -> String {
        if key.is_zero() {
            "NULL".to_string()
        } else {
            hex::encode(key)
        }
    }

    #[inline]
    pub(super) fn hex_key_opt(key: Option<&[u8; 32]>) -> String {
        if let Some(key) = key {
            hex::encode(key)
        } else {
            "NULL".to_string()
        }
    }

    #[inline]
    pub(super) fn hex_pub_key(private_key: &[u8; 32]) -> String {
        if private_key.is_zero() {
            "NULL".to_string()
        } else {
            let private_key = PrivateKey::from(*private_key);
            let public_key = crypto_scalarmult_curve25519_base(&private_key);
            hex_key(public_key.raw())
        }
    }
}

mod tests {
    use crate::cffi;
    use crate::interface::wire::message::Message;
    use std::os::raw::c_char;

    fn mk_msg(padding: usize) -> Message {
        unsafe {
            let alloc =
                cffi::MallocAllocator__new((padding as u64) + 256, "".as_ptr() as *const c_char, 0);
            Message::from_c_message(cffi::Message_new(0, padding as u32, alloc))
        }
    }

    #[test]
    pub fn test_encrypt_decrypt_rnd_nonce() {
        // The message
        const TEST_STRING: &[u8] = b"Hello World";
        let mut msg1 = mk_msg(128);
        let mut msg2 = mk_msg(128);
        msg1.push_bytes(TEST_STRING).unwrap();
        msg2.push_bytes(TEST_STRING).unwrap();

        // Encrypt
        let nonce = [0_u8; 24];
        let secret = [0_u8; 32];
        super::encrypt_rnd_nonce(nonce, &mut msg1, secret);
        unsafe {
            cffi::CryptoAuth_encryptRndNonce(
                nonce[..].as_ptr(),
                msg2.as_c_message(),
                secret[..].as_ptr(),
            );
        }
        println!("Rust: {}", hex::encode(msg1.bytes()));
        println!("C:    {}", hex::encode(msg2.bytes()));
        assert_eq!(msg1.bytes(), msg2.bytes(), "Encrypt results are different");

        // Decrypt
        let nonce = [0_u8; 24];
        let secret = [0_u8; 32];
        let res = super::decrypt_rnd_nonce(nonce, &mut msg1, secret);
        assert!(res.is_ok(), "Decrypt (Rust) failed");
        let res = unsafe {
            cffi::CryptoAuth_decryptRndNonce(
                nonce[..].as_ptr(),
                msg2.as_c_message(),
                secret[..].as_ptr(),
            )
        };
        assert_eq!(res, 0, "Decrypt (C) failed, err_code = {}", res);
        println!("Rust: {}", hex::encode(msg1.bytes()));
        println!("C:    {}", hex::encode(msg2.bytes()));
        assert_eq!(msg1.bytes(), msg2.bytes(), "Results are different");

        // Ensure the message is the same
        assert_eq!(msg1.len(), 11);
        assert_eq!(msg2.len(), 11);
        assert_eq!(msg1.pop_bytes(11).unwrap(), TEST_STRING);
        assert_eq!(msg2.pop_bytes(11).unwrap(), TEST_STRING);
    }
}