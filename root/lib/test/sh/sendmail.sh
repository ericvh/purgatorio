# sendmail talks SMTP to test/netsrv smtp, a server on a loopback port
# that logs what it is sent and refuses nobody@...
load std
d=/tmp/sh-sendmail
rm -rf $d
mkdir -p $d
s=`{test/netsrv smtp $d/log}
srv=tcp!127.0.0.1!^${hd $s}

# Recipients from the To: and Cc: lines; a leading dot is doubled.
echo 'From: Alice <alice@example.org>
To: bob@example.org, Carol <carol@example.net>
Cc: dave@example.com
Subject: test

Hello.
.a line with a dot
To: not@a.header
' | sendmail -s $srv
cat $d/log
rm $d/log

# Recipients as arguments; the sender is the user, in $DOMAIN.
DOMAIN=example.org
echo 'Subject: args

body' | sendmail -s $srv eve@example.org
sed -n 's/^MAIL FROM:<[^@]*@/MAIL FROM:<user@/p; /^RCPT/p' $d/log
rm $d/log

# Failures.
echo 'To: nobody@example.org

x' | sendmail -s $srv
echo $status
echo 'To: a@b

x' | sendmail -s $srv c@d
echo 'From: x@y

x' | sendmail -s $srv
echo x | sendmail -s tcp!127.0.0.1!1 a@b
kill ${tl $s}
rm -rf $d
