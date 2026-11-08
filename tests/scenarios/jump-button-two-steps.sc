# Scrolled back in history, news arrives. The button goes to the first unread message, and
# once the user has been there, to the bottom.
settle
fx seed who=1 n=100
settle
open 1
settle
wheel 650 350 20
settle
expect ui near-bottom = false
fx recv who=1 n=40
settle
expect ui open-unread = 40
jump
settle
expect ui near-bottom = false
expect ui divider-top >= -10
expect ui divider-top <= 12
expect ui open-unread < 40
jump
settle
expect ui near-bottom = true
expect ui bottom-gap = 0
expect ui open-unread = 0
