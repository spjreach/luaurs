-- tostring with arguments of known type
local function f(n)
  local out = {}
  for i = 1, n do
    tostring(nil) tostring("s") tostring(i) tostring(true)
    local a, b, c, d = tostring(nil), tostring("str"), tostring(i / 4), tostring(i % 2 == 0)
    local e = tostring(setmetatable({}, {__tostring = function() return "obj" .. i end}))
    out[#out + 1] = a .. b .. c .. d .. e .. tostring(7i)
  end
  return out
end
for _ = 1, 3 do f(100) end
print(table.concat(f(5), " "))
-- a script that replaces tostring: no builtin path may be used
local env = setmetatable({tostring = function(v) return "<" .. type(v) .. ">" end}, {__index = _G})
local g = setfenv(function() local s = "" for i = 1, 3 do s ..= tostring(nil) .. tostring("x") end return s end, env)
for _ = 1, 50 do g() end
print(g())
