-- CONCAT from native code: numbers, long strings, metamethods, errors
local mt = {__concat = function(a, b)
  local x = type(a) == "table" and a.v or a
  local y = type(b) == "table" and b.v or b
  return {v = x .. "+" .. y}
end}
local function f(n)
  local out = {}
  for i = 1, n do
    local s = "j=" .. i .. "," .. i / 4 .. ";" .. string.rep("x", i % 7)
    local long = string.rep("ab", 300) .. i
    local o = setmetatable({v = "o"}, mt) .. i
    out[#out + 1] = s .. #long .. o.v
  end
  return out
end
for _ = 1, 3 do f(100) end
for _, s in f(5) do print(s) end
print(pcall(function() return "a" .. {} end))
print(pcall(function() local t = nil return "a" .. t end))
