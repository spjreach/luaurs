-- table constructors: SETLIST stores known non-collectable values directly
local function three() return 7, 8, 9 end
local function make(i)
  local a = {i, i + 1, true, false, nil, 2.5, vector.create(i, 0, 0), 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20}
  local b = {i, "s", {}, i * 2}
  local c = {1, 2, three()}
  local d = {}
  for k = 1, 3 do d[k] = {k, k + i} end
  return a, b, c, d
end
local acc = 0
for i = 1, 300 do
  local a, b, c, d = make(i)
  acc += a[1] + a[2] + a[6] + a[18] + #b + b[4] + #c + c[5] + d[3][2]
end
print(acc)
local a, b, c, d = make(5)
for _, t in {a, b, c} do
  local s = {}
  for k = 1, 18 do s[k] = typeof(t[k]) == "table" and "table" or tostring(t[k]) end
  print(table.concat(s, " "))
end
