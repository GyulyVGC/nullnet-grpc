#!/bin/bash

git pull && \
cargo b --release && \
sudo cp nullnet-server.service /etc/systemd/system/ && \
sudo systemctl enable nullnet-server && \
sudo systemctl restart nullnet-server
