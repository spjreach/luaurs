local out = {}
local b = buffer.create(32)
for _, v in {0, 1, -1, 255, 256, 65535, 65536, -129, 2^31, 2^32 + 3, -2^40, 1.75, -1.75, 0/0, 1e300, 3.4e38, 1/3} do
  buffer.writeu8(b, 0, v) buffer.writeu16(b, 2, v) buffer.writeu32(b, 4, v) buffer.writef32(b, 8, v) buffer.writef64(b, 16, v)
  out[#out+1] = table.concat({buffer.readu8(b, 0), buffer.readi8(b, 0), buffer.readu16(b, 2), buffer.readi16(b, 2), buffer.readu32(b, 4), buffer.readi32(b, 4), tostring(buffer.readf32(b, 8)), tostring(buffer.readf64(b, 16))}, " ")
end
for _, off in {-1, 29, 31, 32, 1.5, 2^33} do
  out[#out+1] = tostring(pcall(buffer.readu32, b, off)) .. " " .. tostring(pcall(buffer.writeu32, b, off, 7))
end
local sum = 0 for i = 0, 7 do buffer.writeu32(b, i * 4, i * 1000) end for i = 0, 7 do sum += buffer.readu32(b, i * 4) end out[#out+1] = sum
print(table.concat(out, "\n"))
