local byte, unpack, sqrt = string.byte, table.unpack, math.sqrt
local function count(...) return select("#", ...) end
local function mres(n)
  local s = 0
  for i = 1, n do
    local a, b, c = string.byte("xyz", 1, 3)
    s = s + a + b + c + select("#", string.byte("abcd", 1, i % 4 + 1))
    local q = {byte("hello", 1, -1)}
    s = s + #q + count(byte("ab", 1, i % 2 + 1)) + count(sqrt(i))
    local t = {i, i + 1, i + 2}
    local x, y, z = unpack(t)
    s = s + x + y + z + count(unpack(t, 1, i % 3 + 1)) + count(string.find("hello world", "(o)(r)"))
  end
  return s
end
print(mres(5000))
