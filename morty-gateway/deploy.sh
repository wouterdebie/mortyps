#!/bin/bash
cargo espflash flash --release -p /dev/cu.usbmodem12201 --monitor
