#!/bin/bash
cd /workspace/zeroclaw/ev11457/files
exec env -i HOME=/workspace/zeroclaw/ev11457/home PATH=/usr/bin:/bin TERM=xterm-256color LANG=C.UTF-8 COLORTERM=truecolor /workspace/zeroclaw/ev11457/bin/zerocode --config-dir /workspace/zeroclaw/ev11457/cfg
