# Recorded Eye Tracker 5 messages

These files are copied unchanged from
[Skyggedans/tobii-linux](https://github.com/Skyggedans/tobii-linux)
(`crates/tobii-proto/fixtures`, release v0.6.0), MIT licence, see
`LICENSE-skyggedans`. Each is one USB message as a line of hex, captured from
the Windows Stream Engine talking to a Tobii Eye Tracker 5. The device serial
number was replaced by `IS50F-000000000000` upstream.

The first 8 bytes of each message are the USB prefix. The game DLL's TCP route
carries the same messages without it.
