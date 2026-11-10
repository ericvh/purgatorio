# mcpd stage 2: a job, the namespace tools and a resource, over a pipe.
# The job_start reply (it holds a pid) and the initialize reply are left
# out. The waiting job_read comes last: its reply is the only one that may
# overtake others.
{
	echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"sh","version":"0"}}}'
	echo '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"job_start","arguments":{"argv":["cat"]}}}'
	echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run","arguments":{"command":"mkdir /tmp/u; echo f > /tmp/u/f"}}}'
	echo '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bind","arguments":{"from":"/tmp/u","to":"/n"}}}'
	echo '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"bind","arguments":{"from":"#C","to":"/n"}}}'
	echo '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"run","arguments":{"command":"ls /n"}}}'
	echo '{"jsonrpc":"2.0","id":7,"method":"resources/read","params":{"uri":"purg:///n/f"}}'
	echo '{"jsonrpc":"2.0","id":8,"method":"resources/read","params":{"uri":"purg:///n/nope"}}'
	echo '{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"job_write","arguments":{"id":1,"data":"hello\n"}}}'
	echo '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"job_read","arguments":{"id":1,"wait_ms":60000}}}'
} | mcpd | grep -v '"id":[12],'
echo status $status
