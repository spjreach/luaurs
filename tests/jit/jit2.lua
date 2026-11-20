local out = {}
local function p(...) local t = table.pack(...) for i = 1, t.n do t[i] = tostring(t[i]) end out[#out+1] = table.concat(t, " ") end
local nan = 0/0
for _, v in {nil, false, true, 0, 1, -0.0, nan, "a", "b", {}} do
  p(v == nil, v ~= nil, v == true, v ~= false, v == 0, v ~= 1, v == "a", v ~= "b", not v, not not v)
  if v then p("truthy") else p("falsy") end
  if v == 1 then p("one") elseif v == "a" then p("isa") end
end
for _, a in {1, 2, nan, -1} do for _, b in {1, 2, nan} do p(a < b, a <= b, not (a < b), not (a <= b), a > b, a >= b) end end
local obj = {x = 1, y = "s"}
p(obj.x, obj.y, obj.z) obj.x = 5 obj.z = {} p(obj.x, type(obj.z))
local mt = setmetatable({}, {__index = function(t, k) return k .. "!" end, __newindex = function(t, k, v) rawset(t, k, v * 2) end})
p(mt.foo) mt.bar = 21 p(mt.bar, rawget(mt, "bar"))
local fr = table.freeze({a = 1, 1, 2})
p(pcall(function() fr.a = 2 end)) p(pcall(function() fr[1] = 3 end)) p(fr.a, fr[1])
local arr = {1, 2, 3} arr[2] = "two" arr[4] = 4 p(arr[1], arr[2], arr[3], arr[4], arr[1.5], arr[0], #arr)
G_A = 10 local function g() return G_A end p(g()) G_A = 20 p(g())
p(math.floor(3.7), string.len("abc"), table.concat({1, 2}, ","))
local env = setmetatable({}, {__index = _G}) local function h() return math.max(1, 2), print ~= nil end setfenv(h, env) p(h())
env.math = {max = function() return "patched" end} p(h())
local uv = 1 local function inc() uv += 1 return uv end p(inc(), inc(), uv)
local function mk() local c = 0 return function() c += 1 return c end end local f1, f2 = mk(), mk() p(f1(), f1(), f2())
local s = 0 for i = 10, 1, -2 do s += i end p(s) for i = 1, 0 do p("never") end for i = 1.5, 3 do p(i) end
local function va(...) return select("#", ...), ... end p(va(1, nil, 3))
local co = coroutine.create(function(a) local b = coroutine.yield(a + 1) return b * 2 end)
p(coroutine.resume(co, 1)) p(coroutine.resume(co, 5)) p(coroutine.status(co))
p(pcall(function() return nil + 1 end)) p(pcall(function() local t = nil return t.x end))
p(pcall(error, {code = 1}))
local big = {} for i = 1, 100 do big[i] = i end local sum = 0 for i = 1, #big do sum += big[i] end p(sum)
for k, v in pairs({a = 1}) do p(k, v) end
print(table.concat(out, "\n"))
