# As in Signal Desktop: 15 seconds without input in the window and nothing is read, though it
# has the focus. Input brings the user back, and what is on screen is read a second later.
idle 15000
settle
fx seed who=1 n=100 unread=30
settle
open 1
settle
expect ui active = false
expect ui open-unread = 30
expect backend unread who=1 = 30
expect backend sent who=1 = delivery=0 read=0 sync=0
move 500 300
settle
expect ui active = true
expect backend unread who=1 = 30
idle 1000
settle
expect ui open-unread < 30
