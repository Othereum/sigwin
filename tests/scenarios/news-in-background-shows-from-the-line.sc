# At the bottom, the window loses the focus and two messages arrive: the list goes to the
# line above them and the last message is shown whole, not cut off at the bottom.
settle
fx seed who=1 name=철수 n=100
settle
open 1
settle
expect ui near-bottom = true
expect ui bottom-gap = 0
focus off
settle
fx recv who=1 n=2
settle
expect backend unread who=1 = 2
expect ui open-unread = 2
expect ui bottom-gap = 0
expect ui divider-top > 0
# coming back reads them
focus on
settle
expect backend unread who=1 = 0
expect ui open-unread = 0
expect backend sent who=1 = delivery=2 read=2 sync=2

# more news than fits on screen: the list starts at the line, so the news is read from its start
focus off
settle
fx recv who=1 n=40
settle
expect ui open-unread = 40
expect ui divider-top >= -10
expect ui divider-top <= 12
expect ui bottom-gap > 10
