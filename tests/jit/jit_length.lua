-- #t compiled as a direct luaH_getn call for tables without a metatable
local function lens(t, n)
  local acc = 0
  for i = 1, n do
    t[#t + 1] = i
    acc += #t
  end
  return acc, #t
end
print(lens({}, 1000))
print(lens({1, 2, 3, nil, 5}, 10))
local h = {} for i = 1, 20 do h[i] = i end h[10] = nil
print(lens(h, 5))
local m = setmetatable({}, {__len = function() return 42 end, __newindex = function(t, k, v) rawset(t, k, v) end})
print(lens(m, 3))
local m2 = setmetatable({1, 2}, {})
print(lens(m2, 3))
local function slen(s) local acc = 0 for i = 1, 10 do acc += #s end return acc end
print(slen("hello"), slen(""))
print(pcall(function() local x = nil return #x end))
local function mixed(v) return #v end
for _ = 1, 100 do mixed({1, 2}) end
print(mixed("abc"), mixed({1, 2, 3}), pcall(mixed, 5))
