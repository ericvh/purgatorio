# date in local time (daytime(2)): /locale/timezone by default, which is
# GMT; $timezone holding a file's name or a zone's text; -u for GMT.
date 1794573296
date -u 1794573296
sed 1q /locale/US_Eastern
timezone=/locale/US_Eastern
date 1794573296
date 1783166400
date -u 1783166400
# Daylight time starts at 2:00 EST on 2026-03-08 and ends at 2:00 EDT on
# 2026-11-01.
date 1772953199
date 1772953200
date 1793512799
date 1793512800
# The first period in the file, in 1970.
date 10000000
# The southern hemisphere: daylight time in January.
timezone=/locale/Australia_NSW
date 1768478400
date 1794573296
timezone=/locale/Asia_Kolkata
date 1794573296
# The zone's text itself, as a list: sh writes a list to /env as its
# words, where one quoted word would keep its quotes.
timezone=(JST 32400 JST 32400)
date 1794573296
# Too few words, or a file that is not there: GMT.
timezone='EST -18000'
date 1794573296
timezone=/locale/nosuch
date 1794573296
timezone=()
date 1794573296
