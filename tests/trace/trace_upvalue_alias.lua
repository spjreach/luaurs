local function run(n)
  local x = 0
  local function get() return x end
  local s = 0
  for i = 1, n do x = x + 1; s = s + get() end
  return s
end
for k = 1, 4 do print(run(1000)) end
