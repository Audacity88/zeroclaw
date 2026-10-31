#!/bin/bash
cd /workspace/zeroclaw/ev11529/files
exec env -i HOME=/workspace/zeroclaw/ev11529/home PATH=/usr/bin:/bin TERM=xterm-256color LANG=C.UTF-8 COLORTERM=truecolor DISPLAY=:15 /workspace/zeroclaw/ev11529/bin/zerocode --config-dir /workspace/zeroclaw/ev11529/cfg
