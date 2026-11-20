-- builtins the JIT doesn't expand are called directly from native code
local function f(n)
  local out = {}
  for i = 1, n do
    local t = {6, 9, 7}
    setmetatable(t, { __newindex = function(t, k, v) rawset(t, k * 10, v) end })
    t[1] = 17
    t[5] = i
    out[#out + 1] = tostring(t[1]) .. "," .. tostring(t[5]) .. "," .. tostring(t[50])
    out[#out + 1] = tostring(nil) .. tostring(i) .. tostring("s") .. tostring(i / 3)
    out[#out + 1] = tostring(rawequal(t, t)) .. tostring(rawlen(t)) .. tostring(type(t)) .. tostring(typeof(i))
    local x, y = math.modf(i + 0.25)
    out[#out + 1] = tostring(x) .. tostring(y) .. tostring(string.byte("abc", 2)) .. tostring(select("#", 1, 2, 3))
    local s = setmetatable({}, { __tostring = function() return "custom" .. i end })
    out[#out + 1] = tostring(s) -- a metamethod: the builtin declines
    out[#out + 1] = tostring(pcall(setmetatable, 1, {})) .. tostring(pcall(rawset, 1, 2, 3))
  end
  return out
end
for _ = 1, 3 do f(50) end
local out = f(5)
for _, s in out do print(s) end
