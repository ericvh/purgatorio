# Exit statuses, && || and !.
echo first && echo second
cat /nonexistent || echo recovered
echo $status
cat /nonexistent && echo not reached
echo status is $status
! cat /nonexistent && echo negated
! echo hi
echo $status
{ cat /nonexistent; echo block } && echo block ok
