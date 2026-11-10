# A message request: the request bar replaces the composer, and when the phone blocks the
# sender the chat closes and leaves the list. (An empty value after the operator: nothing open.)
settle
fx phone-storage who=9 accepted=1
settle
fx recv who=1 n=3
settle
expect backend request who=1 = true
open 1
settle
expect ui current-key != 
expect ui current-request = true
fx phone-storage who=1 blocked=1
settle
expect ui current-key = 
expect ui current-request = false
expect backend window who=1 = closed
# read on screen: our devices heard it, the sender heard nothing
fx recv who=1 n=1
settle
expect backend sent who=1 = delivery=0 read=0 sync=3
