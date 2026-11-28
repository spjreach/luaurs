-- %.Nf formatting (exact integer path), upper / lower / reverse in traces
local vals = { 0, -0.0, 0.5, 1.5, 2.5, 0.125, 2.675, 1.005, -3.14159, 1e15, 2^53, 2^63, 1e-7, 5e-324, 123.456 }
local function f(n)
  local h, last = 0, ""
  for i = 1, n do
    local v = vals[(i % #vals) + 1] * ((i % 3) + 1)
    local s = string.format("%.2f %.0f %.5f %f %8.3f %-6.1f %+.2f %.9f", v, v, v, v, v, v, v, v)
    for k = 1, #s, 3 do h = (h * 31 + s:byte(k)) % 1000003 end
    last = s
  end
  return h, last
end
print(f(3000))
local words = { "Hello", "WORLD", "MiXeD cAsE 123", "", "\0a\0B", ("Ab"):rep(200) }
local function g(n)
  local h = 0
  for i = 1, n do
    local w = words[(i % #words) + 1]
    local u, l, r = w:upper(), string.lower(w), w:reverse()
    h = (h + #u * 7 + #l * 3 + #r) % 1000003
    if u:byte(1) then h = (h + u:byte(1) + (l:byte(-1) or 0) + (r:byte(1) or 0)) % 1000003 end
    if i % 700 == 0 then collectgarbage() end
  end
  return h, ("abc"):upper(), ("ABC"):lower(), ("abc"):reverse(), words[3]:upper(), words[6]:upper():sub(1, 4)
end
print(g(5000))
