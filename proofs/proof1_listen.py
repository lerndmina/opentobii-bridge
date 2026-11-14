import socket, selectors, time, sys
DUR=float(sys.argv[1]) if len(sys.argv)>1 else 40
t0=time.time()
def now(): return f"{time.time()-t0:6.2f}s"
sel=selectors.DefaultSelector()
srv=socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR,1); srv.bind(("127.0.0.1",4455)); srv.listen(8); srv.setblocking(False)
udp=socket.socket(socket.AF_INET, socket.SOCK_DGRAM); udp.bind(("127.0.0.1",4457)); udp.setblocking(False)
sel.register(srv,selectors.EVENT_READ,"srv"); sel.register(udp,selectors.EVENT_READ,"udp")
n=0; print(now(),"listening on 127.0.0.1:4455 tcp and 4457 udp",flush=True)
while time.time()-t0<DUR:
    for key,_ in sel.select(timeout=0.5):
        if key.data=="srv":
            c,a=srv.accept(); n+=1; c.setblocking(False); sel.register(c,selectors.EVENT_READ,("c",n)); print(now(),f"conn#{n} accepted from {a}",flush=True)
        elif key.data=="udp":
            d,a=udp.recvfrom(4096); print(now(),f"udp {len(d)}B from {a}: {d[:64].hex()}",flush=True)
        else:
            c=key.fileobj; i=key.data[1]
            try: d=c.recv(4096)
            except OSError as e: d=b""; print(now(),f"conn#{i} error {e}",flush=True)
            if d: print(now(),f"conn#{i} recv {len(d)}B: {d[:96].hex()}",flush=True)
            else: print(now(),f"conn#{i} closed by peer",flush=True); sel.unregister(c); c.close()
print(now(),f"done, {n} connections",flush=True)
