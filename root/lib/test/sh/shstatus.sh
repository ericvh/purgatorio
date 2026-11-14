# $status as Inferno's sh sets it (failurestatus and waitfor in its
# sh.y): a prog's `fail:` and the blanks after it are dropped, an empty
# failure reads `failed`, and other statuses are kept whole. A pipeline's
# status lists every stage's, the last stage's first, if any failed. sh
# exits with `fail:` and its status.
load std

# kill exits `fail: usage`.
kill >[2] /dev/null
echo kill $status
kill >[2] /dev/null
if {~ $status usage} {echo matched usage}
@{raise ' spaced'}
echo raise spaced $status
@{raise ''}
echo raise empty $status
@{exit three}
echo exit three $status
sh -c 'kill' >[2] /dev/null
echo sh -c kill $status
sh -e -c 'kill >[2] /dev/null; echo not reached'
echo sh -e $status

# Pipelines.
@{raise one} | @{raise two}
echo both $status
@{raise one} | cat
echo first $status
echo x | @{cat > /dev/null; raise two}
echo last $status
echo x | cat
echo none $status

# std's status sets the status to its argument as it is.
status fail:kept
echo status $status
status
echo empty $status
