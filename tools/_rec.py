import struct
D="C:/modding/new-project-name/extracted/Export/"
def names(fn):
    d=open(D+fn,"rb").read(); n=struct.unpack(">I",d[:4])[0]//4
    return [d[o:d.index(b"\0",o)].decode("cp1252") for o in struct.unpack(f">{n}I",d[:n*4])]
F=names("Forename.bin"); S=names("Surnames.bin")
p=open(D+"Players.bin","rb").read()
N=27290
recs=[int.from_bytes(p[12+i*40:12+i*40+40],"big") for i in range(N)]
def field(r,pos,w): return (r>>(320-pos-w))&((1<<w)-1)
def name(i): r=recs[i]; return F[field(r,18,14)]+" "+S[field(r,3,15)]
def search(want,minw=3,maxw=16,start=32,tol=0,shifts=(0,)):
    out=[]
    for w in range(minw,maxw+1):
        for pos in range(start,320-w+1):
            vals=[field(recs[i],pos,w) for i in want]
            for k in shifts:
                if all(abs(v-(t+k))<=tol for v,t in zip(vals,want.values())): out.append((pos,w,k,vals))
    return out
