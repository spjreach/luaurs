local function f(x) if x % 50 == 0 then error("bad " .. x) end return x * 2 end
local function pc(n) local s, errs = 0, 0 for i = 1, n do local ok, v = pcall(f, i) if ok then s = s + v else errs = errs + 1 end end return s, errs end
print(pc(10000))
local function xp(n) local s = 0 for i = 1, n do local ok, v = xpcall(f, function(e) return #e end, i) s = s + v end return s end
print(xp(5000))
local function sorts(n) local total = 0 for r = 1, n do local t = {5, 3, 9, 1, r % 7} local desc = r % 2 == 0 table.sort(t, function(a, b) if desc then return a > b end return a < b end) total = total + t[1] * 10 + t[5] end return total end
print(sorts(3000))
local function subs(n) local c = 0 for i = 1, n do local s = string.gsub("a1b2c3", "%d", function(d) c = c + tonumber(d) return "" end) c = c + #s end return c end
print(subs(3000))
local function upv(n) local x, y = 0, 0 local bump = function() x = x + 1 y = y + x end for i = 1, n do pcall(bump) x = x + i end return x, y end
print(upv(5000))
local function grow(n)
  local t = {} for i = 1, 8 do t[i] = i end
  local h = {a = 1, b = 2}
  local s = 0
  for i = 1, n do
    s = s + t[1] + h.a
    pcall(function() for k = 1, 20 do t[#t + 1] = k h["k" .. (i * 20 + k)] = k end end)
    s = s + t[1] + h.a + #t % 7
  end
  return s, #t
end
print(grow(500))
local function deep(d) if d == 0 then return 0 end return 1 + deep(d - 1) end
local function stack(n) local s = 0 for i = 1, n do local ok, v = pcall(deep, 150 + i % 50) s = s + v end return s end
print(stack(2000))
local function multi(...) return ... end
local function mret(n) local s = 0 for i = 1, n do local t = {pcall(multi, i, i + 1, (i % 3 == 0) and i or nil)} s = s + #t end return s end
print(mret(3000))
local function inl(n) local s = 0 local function w(i) return pcall(multi, i, 1) end for i = 1, n do local ok, a, b = w(i) s = s + a + b end return s end
print(inl(5000))
local co = coroutine.wrap(function()
  local s = 0
  for i = 1, 3000 do
    local ok, v = pcall(function() if i % 1000 == 0 then return coroutine.yield(i) end return i end)
    s = s + v
  end
  return "sum " .. s
end)
print(co()) print(co(1)) print(co(2)) print(co(3))
local function err(n) local s = 0 for i = 1, n do local t = {3, 2, 1} table.sort(t, function(a, b) if i == n then error("cmp " .. i) end return a < b end) s = s + t[1] end return s end
print(pcall(err, 500))
local function nested(n) local s = 0 for i = 1, n do pcall(function() for j = 1, 30 do s = s + j end end) end return s end
print(nested(2000))
local function rtypes(n)
  local s = 0 local t = {1, 2, 3}
  for i = 1, n do
    local r = table.find(t, i % 5)
    s = s + (r or 100)
    local ok, v = pcall(function() if i % 9 == 0 then return "x" end return i end)
    if type(v) == "string" then s = s + 1 else s = s + v end
    s = s + #tostring(table.find(t, i % 4))
  end
  return s
end
print(rtypes(5000))
