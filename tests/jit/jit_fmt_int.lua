-- integer conversions of string.format
local fmts = {"%d", "%5d", "%-5d|", "%05d", "%+d", "% d", "%.3d", "%.0d", "%x", "%X", "%#x", "%#X", "%o", "%#o", "%.0o", "%#.0o", "%u", "%08x", "%016x", "%.10x", "%-8X|", "%#10x", "%i"}
local vals = {0, 1, -1, 7, 255, -255, 123456789, 2^31, -2^31, 2^53, 0x7fffffffffffffffi, -0x8000000000000000i, 0i, 255i}
for _, f in fmts do
  local row = {}
  for _, v in vals do
    local ok, r = pcall(string.format, f, v)
    row[#row + 1] = ok and r or "ERR"
  end
  print(f, table.concat(row, ","))
end
print(string.format("%016x%016x", integer.bswap(0x0123456789abcdefi), integer.bswap(-2i)))
