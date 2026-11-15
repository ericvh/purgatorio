# window(1) outside rio: there is no /dev/wctl to write to. Usage errors
# come before that. (rio.rs makes a window with it under rio.)
window echo hi >[2=1]
echo status $status
window -r 1 2 3 >[2=1]
echo status $status
