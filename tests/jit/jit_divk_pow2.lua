-- division by a power-of-two constant is compiled as a multiply; results must be bit-identical
local function f(x)
  return x / 32768, x / 0.5, x / -4, x / 2^-1000, x / 2^1000, x / 3, x / 2^1023, x / 2^-1022, x / 0
end
local inputs = {0, -0, 1, -1, 3.5, 1e-310, 5e-324, 1e308, -1e308, math.huge, -math.huge, 0/0, 12345.678, 2^-1074, 2^1023}
local out = {}
for _ = 1, 300 do
  for i, x in inputs do
    local r = table.pack(f(x))
    local s = {}
    for j = 1, r.n do s[j] = string.format("%a", r[j]) end
    out[i] = table.concat(s, " ")
  end
end
for i = 1, #inputs do print(out[i]) end
