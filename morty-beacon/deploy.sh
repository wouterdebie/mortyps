#!/bin/bash
cargo espflash flash --release -p /dev/cu.usbmodem12301 --monitor
