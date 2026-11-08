# A window that shows without the focus (a second monitor next to a game): news stays at the
# bottom with no line, and is read after it has been on screen for a while, not at once.
settle
fx seed who=1 name=철수 n=100
settle
open 1
settle
shown on
focus off
settle
expect ui inactive = false
fx recv who=1 n=2
settle
expect ui near-bottom = true
expect ui bottom-gap = 0
expect ui divider-top < -1000
expect backend unread who=1 = 2
expect backend sent who=1 = delivery=2 read=0 sync=0
wait 6500
settle
expect backend unread who=1 = 0
expect backend sent who=1 = delivery=2 read=2 sync=2
# covered: back to the background, news is not read
shown off
settle
expect ui inactive = true
fx recv who=1 n=2
settle
wait 6500
settle
expect backend unread who=1 = 2
