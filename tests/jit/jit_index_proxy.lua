-- table accesses on userdata / tables with __index tables, from native code
local vals = {10, 20, nil, 40}
local proxy = newproxy(true)
getmetatable(proxy).__index = vals
local deep = setmetatable({}, {__index = setmetatable({}, {__index = {7, 8}})})
local fnidx = newproxy(true)
getmetatable(fnidx).__index = function(_, k) return k * 100 end
local own = setmetatable({1, 2}, {__index = {5, 6, 7}})
local function f()
  local s = {}
  s[#s + 1] = proxy[1]; s[#s + 1] = proxy[2]; s[#s + 1] = tostring(proxy[3]); s[#s + 1] = proxy[4]
  s[#s + 1] = tostring(proxy[9])
  s[#s + 1] = deep[1]; s[#s + 1] = deep[2]; s[#s + 1] = tostring(deep[3])
  s[#s + 1] = fnidx[1]; s[#s + 1] = fnidx[2]
  s[#s + 1] = own[1]; s[#s + 1] = own[2]; s[#s + 1] = own[3]
  return table.concat(s, " ")
end
for _ = 1, 10 do f() end
vals[3] = 30
print(f())
print(pcall(function() local u = newproxy() return u[1] end))
