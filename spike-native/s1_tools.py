"""S1 静态分析工具集（QQ wrapper.node）

用法示例（在 spike-native 目录）：
  python s1_tools.py fn 0x731EA6          # RVA 所在函数区间
  python s1_tools.py dis 0x73699F 0x736A8B # 反汇编区间（lea 自动注释字符串）
  python s1_tools.py refs 0x443A370        # 全 .text 扫 RIP-lea 引用某 RVA 的代码
  python s1_tools.py callers 0x731EA6      # 直接调用者扫描
"""
import sys, struct, bisect
import pefile
from capstone import Cs, CS_ARCH_X86, CS_MODE_64

NODE = r'D:\200software\203QQ\versions\9.9.36-53644\resources\app\wrapper.node'
pe = pefile.PE(NODE)
data = open(NODE, 'rb').read()
md = Cs(CS_ARCH_X86, CS_MODE_64)
TEXT_S, TEXT_E = 0x1000, 0x4046146

def off(r): return pe.get_offset_from_rva(r)

# .pdata 函数边界（53.7 万条）
for _s in pe.sections:
    if b'.pdata' in _s.Name:
        _raw = data[_s.PointerToRawData:_s.PointerToRawData+_s.SizeOfRawData]; break
FUNCS = []
for _i in range(0, len(_raw)-12, 12):
    _b, _e, _ = struct.unpack('<III', _raw[_i:_i+12])
    if _b and _e > _b: FUNCS.append((_b, _e))
FUNCS.sort()
STARTS = [f[0] for f in FUNCS]

def func_of(rva):
    i = bisect.bisect_right(STARTS, rva) - 1
    return FUNCS[i] if i >= 0 and FUNCS[i][0] <= rva < FUNCS[i][1] else None

def annotate(ins):
    if ins.mnemonic == 'lea' and 'rip' in ins.op_str:
        try:
            d = int(ins.op_str.split('+')[1].strip(),16) if '+' in ins.op_str else -int(ins.op_str.split('-')[1].strip(),16)
            t = ins.address + ins.insn_size + d
            extra = ''
            if 0x4047000 <= t < 0x5E1EDC0:
                oo = off(t); s = data[oo:oo+48].split(b'\0')[0]
                if 4 < len(s) < 48 and all(32 <= c < 127 for c in s):
                    extra = f'   ; "{s.decode()}"'
            return f"0x{ins.address:08X} {ins.mnemonic} {ins.op_str}  ; 0x{t:X}{extra}"
        except Exception: pass
    return f"0x{ins.address:08X} {ins.mnemonic} {ins.op_str}"

def scan_callers(target):
    buf = data[off(TEXT_S):off(TEXT_E)]
    pos, out = 0, []
    while True:
        i = buf.find(b'\xe8', pos)
        if i < 0 or i+5 > len(buf): break
        rel = struct.unpack('<i', buf[i+1:i+5])[0]; t = TEXT_S+i
        if t+5+rel == target: out.append(t)
        pos = i+1
    return out

def scan_refs(target, win=48):
    buf = data[off(TEXT_S):off(TEXT_E)]
    out = []
    for i in range(len(buf)-7):
        b0 = buf[i]
        if b0 in (0x48,0x49,0x4C,0x4D) and buf[i+1] == 0x8D and (buf[i+2]&0xC7) == 0x05:
            d = struct.unpack('<i', buf[i+3:i+7])[0]; t = TEXT_S+i+7+d
        elif b0 == 0x8D and (buf[i+1]&0xC7) == 0x05:
            d = struct.unpack('<i', buf[i+2:i+6])[0]; t = TEXT_S+i+6+d
        else: continue
        if target-win <= t <= target+win: out.append((TEXT_S+i, t))
    return out

if __name__ == '__main__':
    cmd = sys.argv[1]
    if cmd == 'fn':
        print(func_of(int(sys.argv[2], 16)))
    elif cmd == 'dis':
        fb, fe = int(sys.argv[2],16), int(sys.argv[3],16)
        for ins in md.disasm(data[off(fb):off(fb)+(fe-fb)], fb):
            print(annotate(ins))
    elif cmd == 'callers':
        cs = scan_callers(int(sys.argv[2],16))
        print(f"{len(cs)} 个调用点")
        for c in cs:
            f = func_of(c); print(f"0x{c:X} in {f[0]:#x}-{f[1]:#x}" if f else f"0x{c:X} (无pdata)")
    elif cmd == 'refs':
        rs = scan_refs(int(sys.argv[2],16))
        print(f"{len(rs)} 个引用")
        for a,t in rs:
            f = func_of(a); fn = f"func {f[0]:#x}" if f else "?"
            print(f"0x{a:X} ({fn}) -> 0x{t:X}")
