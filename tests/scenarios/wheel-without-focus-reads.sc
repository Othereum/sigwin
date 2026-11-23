# As in Signal Desktop: a window without the focus reads nothing, even with the mouse moving over
# it. The wheel over it (a second monitor) puts the user at it for a second; what the list shows
# is read once that has lasted a second, and a single turn is over before then.
settle
fx seed who=1 name=철수 n=100
settle
open 1
settle
focus off
idle 100
settle
expect ui active = false
fx recv who=1 n=2
settle
expect backend unread who=1 = 2
expect backend sent who=1 = delivery=2 read=0 sync=0
# the mouse moves over it: nothing
move 500 300
settle
expect ui active = false
# one turn of the wheel: at it for a second, too short to read what was already there
wheel 500 300 -1
settle
expect ui active = true
idle 1000
settle
expect ui active = false
expect backend unread who=1 = 2
# turning on: read a second after it started
wheel 500 300 -1
idle 500
wheel 500 300 -1
idle 600
settle
expect ui active = true
expect backend unread who=1 = 0
expect backend sent who=1 = delivery=2 read=2 sync=2
# a second after the last turn it is over
idle 1000
settle
expect ui active = false
fx recv who=1 n=2
settle
expect backend unread who=1 = 2
