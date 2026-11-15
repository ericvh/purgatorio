# std's if (sh-std(1)): if cond action [cond action]... [elseaction].
# The first condition that succeeds runs its action and no other
# condition is tried; with none, the else action; with no else, nothing
# and an empty status.
load std
if {~ a b} {echo no} {~ a a} {echo second} {echo not this}
if {~ a b} {echo no} {~ a c} {echo no} {echo else}
if {echo tried} {echo first} {echo never tried} {echo no}
if {~ a b} {echo no} {~ a c} {echo no}
echo status $status
if {~ a a} >[2=1]
echo status $status
