-- tostring of numbers in traces (direct formatting, a per-site cache that
-- must not outlive the strings it holds)
local vals = { 0, -0.0, 1, -1, 0.5, 1e300, -1e-300, 1 / 0, -1 / 0, 123456789012, 2 ^ 53, 3.14159 }
local function a(n)
  local len, last = 0, ""
  for i = 1, n do
    local s = tostring(vals[(i % #vals) + 1])
    len = len + #s
    last = s
  end
  return len, last
end
print(a(5000))
-- collections while the cache is warm: the strings it hands out are live
local function b(n)
  local t, s = {}, 0
  for i = 1, n do
    local k = tostring((i % 300) + 0.25)
    if i % 997 == 0 then collectgarbage() end
    if i % 3 == 0 then t[#t + 1] = k end
    s = s + #k
  end
  local c = 0
  for _, v in ipairs(t) do if tonumber(v) then c = c + 1 end end
  return s, c, #t, t[#t]
end
print(b(30000))
-- strings that became garbage, then the same numbers again
local function c(n)
  local s = 0
  for r = 1, 5 do
    for i = 1, n do s = s + #tostring(i * 1.5) end
    collectgarbage()
    collectgarbage()
  end
  return s
end
print(c(2000))
-- results used as table keys and compared
local seen, dup = {}, 0
for i = 1, 4000 do
  local k = tostring(i % 50)
  if seen[k] then dup = dup + 1 else seen[k] = true end
end
print(dup, tostring(0 / 0) == tostring(0 / 0))
