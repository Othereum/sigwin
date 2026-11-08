# A thread with unread messages opens at the first one, with the line above it, and only what
# is fully on screen counts as read.
settle
fx seed who=1 name=철수 n=100 unread=30
settle
open 1
settle
expect ui near-bottom = false
expect ui divider-top >= -10
expect ui divider-top <= 12
# the rows on screen were read, the rest wait
expect ui open-unread < 30
expect ui open-unread > 5
expect backend window who=1 = len=60 divider=30 reached=true end=false
