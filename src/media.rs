//! Files and images for a room: encrypted in encrypted rooms, plain in others, with the
//! retries of [`crate::send`].
//!
//! `client.media().upload` followed by an event with a plain `url` puts the file
//! unencrypted on the homeserver, also for an end-to-end encrypted room. An [`Attachment`]
//! asks the room first, as the SDK's `Room::send_attachment` does: encrypted, the file is
//! encrypted before the upload and its key travels in the (encrypted) event's `file`;
//! otherwise it is uploaded as it is and referenced by `url`.
//!
//! Not `Room::send_attachment` itself: it uploads again on every retry of the send, picks
//! `m.audio` / `m.video` by MIME type, keeps the uploaded source to itself and fetches the
//! replied-to event for a relation. Here the upload is retried on its own, done once per
//! kind (plain, encrypted) and reused for further rooms, and the bot can still change the
//! content (a thread relation) or build its own from the [`MediaSource`].

use matrix_sdk::{
    ruma::{
        events::room::{
            message::{
                FileInfo, FileMessageEventContent, ImageMessageEventContent, MessageType,
                RoomMessageEventContent,
            },
            ImageInfo, MediaSource,
        },
        events::MessageLikeEventContent,
        OwnedEventId, TransactionId, UInt,
    },
    Room,
};
pub use mime::Mime;
use tokio::sync::OnceCell;

use crate::send::retry_request;

/// A file or image to post: its name, type and bytes, an optional caption and an image's
/// size in pixels. Uploads itself on the first [`upload`](Self::upload) for a room of each
/// kind and keeps the source for the next rooms.
#[derive(Debug)]
pub struct Attachment {
    file_name: String,
    mime: Mime,
    data: Vec<u8>,
    caption: Option<String>,
    dimensions: Option<(u32, u32)>,
    plain: OnceCell<MediaSource>,
    encrypted: OnceCell<MediaSource>,
}

impl Attachment {
    pub fn new(file_name: impl Into<String>, mime: Mime, data: Vec<u8>) -> Self {
        Self {
            file_name: file_name.into(),
            mime,
            data,
            caption: None,
            dimensions: None,
            plain: OnceCell::new(),
            encrypted: OnceCell::new(),
        }
    }

    /// The text shown with the file. The event's `body` is the caption and `filename` the
    /// file name (MSC2530); without a caption, `body` is the file name.
    pub fn caption(mut self, caption: impl Into<String>) -> Self {
        self.caption = Some(caption.into());
        self
    }

    /// An image's width and height in pixels; 0 means unknown and is left out.
    pub fn dimensions(mut self, width: u32, height: u32) -> Self {
        self.dimensions = Some((width, height));
        self
    }

    pub fn mime(&self) -> &Mime {
        &self.mime
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// The size in bytes.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Upload the file for `room`: encrypted when the room is encrypted, plain otherwise
    /// (asked of the server when the bot does not know yet; an error then, never a plain
    /// upload). Transient failures are retried as sends are. Once per kind: a second room
    /// of the same kind gets the same source.
    pub async fn upload(&self, room: &Room) -> Result<MediaSource, matrix_sdk::Error> {
        let encrypted = retry_request(room, || async {
            Ok(room.latest_encryption_state().await?.is_encrypted())
        })
        .await?;
        let cell = if encrypted {
            &self.encrypted
        } else {
            &self.plain
        };
        cell.get_or_try_init(|| retry_request(room, || self.upload_once(room, encrypted)))
            .await
            .cloned()
    }

    async fn upload_once(
        &self,
        room: &Room,
        encrypted: bool,
    ) -> Result<MediaSource, matrix_sdk::Error> {
        let client = room.client();
        if encrypted {
            let mut reader = self.data.as_slice();
            let file = client.upload_encrypted_file(&mut reader).await?;
            Ok(MediaSource::Encrypted(Box::new(file)))
        } else {
            let response = client
                .media()
                .upload(&self.mime, self.data.clone(), None)
                .await?;
            Ok(MediaSource::Plain(response.content_uri))
        }
    }

    /// The event for the uploaded file: `m.image` for an `image/*` type, `m.file` for
    /// everything else, with `info` (MIME type, size, an image's width and height).
    pub fn content(&self, source: MediaSource) -> RoomMessageEventContent {
        let (body, filename) = match &self.caption {
            Some(caption) => (caption.clone(), Some(self.file_name.clone())),
            None => (self.file_name.clone(), None),
        };
        let mimetype = Some(self.mime.to_string());
        let size = known(self.data.len() as u64);
        let msgtype = if self.mime.type_() == mime::IMAGE {
            let (width, height) = self.dimensions.unwrap_or((0, 0));
            let mut info = ImageInfo::new();
            info.mimetype = mimetype;
            info.size = size;
            info.width = known(width.into());
            info.height = known(height.into());
            let mut content = ImageMessageEventContent::new(body, source);
            content.filename = filename;
            content.info = Some(Box::new(info));
            MessageType::Image(content)
        } else {
            let mut info = FileInfo::new();
            info.mimetype = mimetype;
            info.size = size;
            let mut content = FileMessageEventContent::new(body, source);
            content.filename = filename;
            content.info = Some(Box::new(info));
            MessageType::File(content)
        };
        RoomMessageEventContent::new(msgtype)
    }
}

/// A positive number for `info`; 0 (unknown) is left out.
fn known(n: u64) -> Option<UInt> {
    (n > 0).then(|| UInt::new_saturating(n))
}

/// Upload `data` for `room` (see [`Attachment::upload`]) for a bot that builds the event
/// itself: an [`ImageMessageEventContent::new`] or [`FileMessageEventContent::new`] with the
/// source, never `plain` with an mxc URI.
pub async fn upload(
    room: &Room,
    mime: &Mime,
    data: Vec<u8>,
) -> Result<MediaSource, matrix_sdk::Error> {
    Attachment::new(String::new(), mime.clone(), data)
        .upload(room)
        .await
}

/// Upload `attachment` for `room` (see [`Attachment::upload`]) and post it, retried as
/// [`crate::send::send_with_retry`] is.
pub async fn send(room: &Room, attachment: &Attachment) -> Result<OwnedEventId, matrix_sdk::Error> {
    send_txn(room, attachment, &TransactionId::new()).await
}

/// [`send`] with a transaction ID chosen by the caller (see
/// [`crate::send::send_with_retry_txn`]): a repeat after a restart uploads again but posts
/// no second event.
pub async fn send_txn(
    room: &Room,
    attachment: &Attachment,
    txn_id: &TransactionId,
) -> Result<OwnedEventId, matrix_sdk::Error> {
    send_content_txn(
        room,
        attachment,
        |source| attachment.content(source),
        txn_id,
    )
    .await
}

/// [`send_txn`] for a bot that builds the event itself (a thread relation, its own `info`):
/// `content` makes it from the source, for every attempt.
///
/// Encryption can be switched on while the upload and its retries run, and the send encrypts
/// the event by the room's state when it goes out: an encrypted event pointing at a file
/// uploaded in plaintext. So the source is asked for again right before every attempt, from
/// the state the send itself goes by: a room encrypted by now gets the file uploaded again,
/// encrypted (cached like the first). Never the other way round, encryption is not switched
/// off again.
pub async fn send_content_txn<C>(
    room: &Room,
    attachment: &Attachment,
    content: impl Fn(MediaSource) -> C,
    txn_id: &TransactionId,
) -> Result<OwnedEventId, matrix_sdk::Error>
where
    C: MessageLikeEventContent,
{
    // The upload first, with its own retries; failing there is not a failed send.
    attachment.upload(room).await?;
    retry_request(room, || async {
        let source = attachment.upload(room).await?;
        room.send(content(source))
            .with_transaction_id(txn_id.to_owned())
            .await
            .map(|response| response.response.event_id)
    })
    .await
}

#[cfg(test)]
mod tests {
    use matrix_sdk::ruma::{mxc_uri, serde::Raw};
    use serde_json::{json, Value};

    use super::*;

    fn json_of(content: &RoomMessageEventContent) -> Value {
        Raw::new(content)
            .unwrap()
            .deserialize_as_unchecked()
            .unwrap()
    }

    fn plain() -> MediaSource {
        MediaSource::Plain(mxc_uri!("mxc://house.org/abc").to_owned())
    }

    fn encrypted() -> MediaSource {
        MediaSource::Encrypted(Box::new(
            serde_json::from_value(json!({
                "url": "mxc://house.org/enc",
                "key": {
                    "kty": "oct", "key_ops": ["encrypt", "decrypt"], "alg": "A256CTR",
                    "k": "qcHVMSgYg-71CauWBezXI5qkaRb0LuIy-Wx5kIaHMIA", "ext": true
                },
                "iv": "X85+XgHN+HEAAAAAAAAAAA",
                "hashes": { "sha256": "5qG4fFnbbVdlAB1Q72JDKwCagV6Dbkx9uds4rSak37c" },
                "v": "v2"
            }))
            .unwrap(),
        ))
    }

    #[test]
    fn an_image_with_a_caption_has_the_caption_as_body_and_its_size() {
        let image = Attachment::new("map.png", mime::IMAGE_PNG, vec![1; 2048])
            .caption("🗺️ Guess 1/5")
            .dimensions(800, 600);
        assert_eq!(
            json_of(&image.content(plain())),
            json!({
                "msgtype": "m.image",
                "body": "🗺️ Guess 1/5",
                "filename": "map.png",
                "url": "mxc://house.org/abc",
                "info": { "mimetype": "image/png", "size": 2048, "w": 800, "h": 600 }
            })
        );
    }

    #[test]
    fn unknown_dimensions_are_left_out() {
        let image = Attachment::new("x.jpg", mime::IMAGE_JPEG, vec![1; 3]).dimensions(0, 0);
        let content = json_of(&image.content(plain()));
        assert_eq!(content["body"], "x.jpg");
        assert!(content.get("filename").is_none());
        assert_eq!(
            content["info"],
            json!({ "mimetype": "image/jpeg", "size": 3 })
        );
    }

    #[test]
    fn anything_but_an_image_is_a_file_even_audio_and_video() {
        for (name, mime) in [
            ("a.pdf", mime::APPLICATION_PDF),
            ("a.mp3", "audio/mpeg".parse().unwrap()),
            ("a.mp4", "video/mp4".parse().unwrap()),
        ] {
            let file = Attachment::new(name, mime.clone(), vec![0; 10]);
            let content = json_of(&file.content(plain()));
            assert_eq!(content["msgtype"], "m.file", "{name}");
            assert_eq!(content["body"], name);
            assert_eq!(content["info"]["mimetype"], mime.to_string());
        }
    }

    #[test]
    fn an_encrypted_source_goes_in_file_and_no_plain_url() {
        let image = Attachment::new("x.png", mime::IMAGE_PNG, vec![1; 3]);
        let content = json_of(&image.content(encrypted()));
        assert!(content.get("url").is_none(), "{content}");
        assert_eq!(content["file"]["url"], "mxc://house.org/enc");
        assert_eq!(content["file"]["key"]["alg"], "A256CTR");
        assert_eq!(content["msgtype"], "m.image");
    }

    /// Against the SDK's mock homeserver (feature `testing`).
    #[cfg(feature = "testing")]
    mod with_a_server {
        use matrix_sdk::{
            ruma::{event_id, room_id},
            test_utils::mocks::MatrixMockServer,
        };

        use super::*;

        /// A logged-in client; the upload size limit is asked before every upload.
        async fn homeserver() -> (MatrixMockServer, matrix_sdk::Client) {
            let server = MatrixMockServer::new().await;
            let client = server.client_builder().build().await;
            server
                .mock_authenticated_media_config()
                .ok_default()
                .mount()
                .await;
            (server, client)
        }

        #[tokio::test]
        async fn a_plain_room_gets_the_file_as_it_is_once_for_two_rooms() {
            let (server, client) = homeserver().await;
            server.mock_room_state_encryption().plain().mount().await;
            let (body, upload) = server
                .mock_upload()
                .expect_mime_type("image/png")
                .ok_with_capture(mxc_uri!("mxc://house.org/abc"));
            upload.expect(1).mount().await;
            server
                .mock_room_send()
                .body_matches_partial_json(json!({
                    "msgtype": "m.image", "url": "mxc://house.org/abc", "body": "x.png"
                }))
                .ok(event_id!("$sent"))
                .expect(2)
                .mount()
                .await;
            let one = server
                .sync_joined_room(&client, room_id!("!one:house.org"))
                .await;
            let two = server
                .sync_joined_room(&client, room_id!("!two:house.org"))
                .await;

            let image = Attachment::new("x.png", mime::IMAGE_PNG, b"PNG bytes".to_vec());
            assert_eq!(send(&one, &image).await.unwrap(), event_id!("$sent"));
            assert_eq!(send(&two, &image).await.unwrap(), event_id!("$sent"));
            assert_eq!(body.await.unwrap(), b"PNG bytes");
        }

        #[tokio::test]
        async fn an_encrypted_room_gets_the_file_encrypted() {
            let (server, client) = homeserver().await;
            server
                .mock_room_state_encryption()
                .encrypted()
                .mount()
                .await;
            let (body, upload) = server
                .mock_upload()
                .expect_mime_type("application/octet-stream")
                .ok_with_capture(mxc_uri!("mxc://house.org/enc"));
            upload.expect(1).mount().await;
            let room = server
                .sync_joined_room(&client, room_id!("!e2e:house.org"))
                .await;

            let image = Attachment::new("x.png", mime::IMAGE_PNG, b"secret picture".to_vec());
            let MediaSource::Encrypted(file) = image.upload(&room).await.unwrap() else {
                panic!("a plain upload into an encrypted room");
            };
            assert_eq!(file.url, mxc_uri!("mxc://house.org/enc"));
            let uploaded = body.await.unwrap();
            assert_ne!(uploaded, b"secret picture");
            assert_eq!(uploaded.len(), b"secret picture".len());
            // the second time from the cache, no second upload (`expect(1)`)
            assert!(matches!(
                image.upload(&room).await.unwrap(),
                MediaSource::Encrypted(_)
            ));
        }

        #[tokio::test]
        async fn encryption_switched_on_during_the_upload_uploads_again_encrypted() {
            use std::time::Duration;

            use matrix_sdk::config::SyncSettings;
            use wiremock::{
                matchers::{method, path},
                Mock, ResponseTemplate,
            };

            let (server, client) = homeserver().await;
            server.mock_room_state_encryption().plain().mount().await;
            let room = server
                .sync_joined_room(&client, room_id!("!late:house.org"))
                .await;
            // The plain upload is slow; encryption is switched on while it runs.
            server
                .mock_upload()
                .expect_mime_type("image/png")
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({ "content_uri": "mxc://house.org/abc" }))
                        .set_delay(Duration::from_millis(500)),
                )
                .expect(1)
                .mount()
                .await;
            let (body, upload) = server
                .mock_upload()
                .expect_mime_type("application/octet-stream")
                .ok_with_capture(mxc_uri!("mxc://house.org/enc"));
            upload.expect(1).mount().await;
            Mock::given(method("GET"))
                .and(path("/_matrix/client/v3/sync"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "next_batch": "s2",
                    "rooms": { "join": { "!late:house.org": {
                        "state": { "events": [{
                            "type": "m.room.encryption",
                            "state_key": "",
                            "content": { "algorithm": "m.megolm.v1.aes-sha2" },
                            "event_id": "$encryption",
                            "sender": "@admin:house.org",
                            "origin_server_ts": 1
                        }] },
                        "timeline": { "events": [] }
                    } } }
                })))
                .with_priority(1)
                .mount(server.server())
                .await;
            // What an encrypted send asks for (no other members, so no keys to share).
            server.mock_get_members().ok(Vec::new()).mount().await;
            server.mock_query_keys().ok().mount().await;
            server
                .mock_room_send()
                .body_matches_partial_json(json!({ "algorithm": "m.megolm.v1.aes-sha2" }))
                .ok(event_id!("$sent"))
                .expect(1)
                .mount()
                .await;

            let image = Attachment::new("x.png", mime::IMAGE_PNG, b"secret picture".to_vec());
            let switch_on = async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                client.sync_once(SyncSettings::default()).await.unwrap();
                assert!(room.encryption_state().is_encrypted());
            };
            let (sent, ()) = tokio::join!(send(&room, &image), switch_on);
            assert_eq!(sent.unwrap(), event_id!("$sent"));
            // The event that went out encrypted points at the encrypted upload (`expect(1)`
            // on both uploads), not at the plaintext one.
            let uploaded = tokio::time::timeout(Duration::from_secs(5), body)
                .await
                .expect("no encrypted upload: the event points at the plaintext file")
                .unwrap();
            assert_ne!(uploaded, b"secret picture");
        }

        #[tokio::test]
        async fn a_failed_upload_is_retried() {
            let (server, client) = homeserver().await;
            server.mock_room_state_encryption().plain().mount().await;
            server
                .mock_upload()
                .error500()
                .up_to_n_times(1)
                .expect(1)
                .mount()
                .await;
            server
                .mock_upload()
                .ok(mxc_uri!("mxc://house.org/abc"))
                .expect(1)
                .mount()
                .await;
            let room = server
                .sync_joined_room(&client, room_id!("!one:house.org"))
                .await;

            let file = Attachment::new("a.pdf", mime::APPLICATION_PDF, vec![1, 2, 3]);
            let MediaSource::Plain(uri) = file.upload(&room).await.unwrap() else {
                panic!("an encrypted upload into a plain room");
            };
            assert_eq!(uri, mxc_uri!("mxc://house.org/abc"));
        }
    }
}
