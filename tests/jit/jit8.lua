local out = {}
local Base = {} Base.__index = Base
function Base.new(x) return setmetatable({x = x}, Base) end
function Base:get() return self.x end
function Base:add(n) self.x += n return self end
local Derived = setmetatable({}, {__index = Base}) Derived.__index = Derived
function Derived.new(x) return setmetatable({x = x, y = 1}, Derived) end
function Derived:gety() return self.y end
local objs = {Base.new(1), Derived.new(2), Base.new(3)}
for i = 1, 30 do for _, o in objs do o:add(1) end end
for _, o in objs do out[#out+1] = o:get() end
out[#out+1] = objs[2]:gety()
local own = {f = function(self, a) return a * 2 end} out[#out+1] = own:f(21)
local fn_index = setmetatable({}, {__index = function(t, k) return function() return "dyn:" .. k end end}) out[#out+1] = fn_index:hello()
out[#out+1] = tostring(pcall(function() local t = {} return t:missing() end))
out[#out+1] = ("abc"):upper() .. (" x "):len()
local shadow = Base.new(5) shadow.get = function() return "own" end out[#out+1] = shadow:get()
local frozen = setmetatable({}, table.freeze({__index = {m = function() return "frozen" end}})) out[#out+1] = frozen:m()
print(table.concat(out, "\n"))
