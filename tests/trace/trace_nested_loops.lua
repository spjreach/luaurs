local function fixed(n)
  local s = 0
  for i = 1, n do
    for j = 1, 4 do s = s + i * j end
  end
  return s
end
print(fixed(100000))
local function varying(n)
  local s = 0
  for i = 1, n do
    for j = 1, (i % 5) do s = s + j end   -- trip count 0..4 changes every iteration
  end
  return s
end
print(varying(100000))
local function down(n)
  local s = 0
  for i = 1, n do
    for j = 3, 1, -1 do s = s + j * i end
    local k = 0
    while k < 3 do k = k + 1; s = s - k end
  end
  return s
end
print(down(100000))
local function triangle(n)   -- n-body style pairs
  local b = {}
  for i = 1, 5 do b[i] = {x = i, v = 0} end
  for step = 1, n do
    for i = 1, 5 do
      local bi = b[i]
      for j = i + 1, 5 do
        local d = b[j].x - bi.x
        bi.v = bi.v + d * 0.001
        b[j].v = b[j].v - d * 0.001
      end
    end
    for i = 1, 5 do b[i].x = b[i].x + b[i].v end
  end
  return b[1].x, b[5].x
end
print(triangle(20000))
local function long(n)   -- inner loop too long to unroll
  local s = 0
  for i = 1, n do for j = 1, 200 do s = s + j end end
  return s
end
print(long(2000))
