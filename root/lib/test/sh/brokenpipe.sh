# A writer whose pipe has lost its reader ends quietly, as in Inferno,
# where sys->print returns -1 and the program carries on to its end.
{sleep 1; ps} | echo reader gone
echo status $status
