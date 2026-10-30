#!/bin/bash
# Run by the daemon's shell tool: opens a ZeroCode client whose process ancestry includes the daemon.
export DISPLAY=:15
D=/workspace/zeroclaw/ev11589
xfce4-terminal --disable-server --title "CHILD zerocode, started by the daemon shell tool" --geometry 120x34+40+40 \
  -x /workspace/zeroclaw/rrbox/t11589/debug/zerocode --config-dir $D/cfg
echo "child terminal exited rc=$?"
