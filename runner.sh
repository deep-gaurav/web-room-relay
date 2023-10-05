#!/bin/bash
rm -f web-room-relay
wget https://github.com/deep-gaurav/web-room-relay/releases/download/nightly/web-room-relay
chmod +x web-room-relay
export RUST_LOG=INFO
exec ./web-room-relay